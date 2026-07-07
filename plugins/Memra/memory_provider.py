"""MAProvider — Memra 的 Hermes MemoryProvider 实现。

架构决策：subprocess 方案（非 in-process）。
原因：MA 依赖 fastembed（未在 Hermes venv 安装），无法 in-process import。
     通过 scripts/mcp_wrapper.sh 启动 MA MCP 服务，走 stdio 协议通信。

通信协议：JSON-RPC 2.0 over stdio（MCP 标准）。
每次工具调用都是短连接（initialize → tools/call → 关闭），
避免长连接带来的进程管理复杂度。

关键 hook 实现：
- system_prompt_block() → 调 get_context(mode=wake) 注入记忆快照
- on_pre_compress()      → 调 get_context(mode=compact) 萃取压缩前上下文
- on_session_end()       → 调 add_rule 存储会话关键事实
- sync_turn()            → daemon thread 队列写（non-blocking，ABC 要求）
"""

from __future__ import annotations

import json
import logging
import os
import queue
import subprocess
import threading
import time
from pathlib import Path
from typing import Any, Dict, List, Optional

logger = logging.getLogger(__name__)

# 基类决策（class 定义前完成，避免运行时 __bases__ 替换的 deallocator 冲突）
# - Hermes 加载本插件时：agent.memory_provider 在 sys.path，继承真 ABC
# - MA 仓库 pytest 跑时：hermes 包不在，fallback 到 object（功能等价 stub）
# 用 importlib 动态 import 避开 pyright 静态解析（ruff 也不会重排）
import importlib  # noqa: E402

try:
    _HermesBase: type = importlib.import_module("agent.memory_provider").MemoryProvider
except (ImportError, AttributeError):
    _HermesBase = object

# MA wrapper 路径（使用绝对路径，避免 PATH 依赖）
_MA_WRAPPER_PATH = str(Path.home() / "projects" / "memra" / "scripts" / "mcp_wrapper.sh")

# get_context 返回内容的最大字符数（ABC 要求 system_prompt_block ≤ 2100 字符）
_MAX_SYSTEM_PROMPT_CHARS = 2100

# subprocess 调用超时（秒）
_TOOL_CALL_TIMEOUT = 15

# sync_turn 后台写入队列大小上限（防止内存无限增长）
_WRITE_QUEUE_MAXSIZE = 100


# ---------------------------------------------------------------------------
# MCP stdio 短连接辅助函数
# ---------------------------------------------------------------------------


def _mcp_call(tool_name: str, arguments: dict, *, timeout: float = _TOOL_CALL_TIMEOUT) -> dict:
    """通过 Popen 调用一次 MA MCP 工具，返回解析后的结果 dict。

    协议：MCP JSON-RPC 2.0 over stdio（长连接语义）。
    流程：spawn → 写 initialize → 写 notifications/initialized → 等 id=1 响应
          → 写 tools/call → 逐行读 stdout 直到收到 id=2 响应 → 关闭进程。

    关键：MA 是 async stdio 服务，stdin 关闭即 EOF 触发进程退出。
    必须用 Popen + 逐行读 stdout，不能用 subprocess.run（它在 stdin 全部写完后
    才开始读，而此时 MA 已因 EOF 退出）。

    失败时记录日志并返回空 dict，不抛异常（避免阻断 Hermes 主流程）。
    """
    wrapper = _MA_WRAPPER_PATH
    if not Path(wrapper).is_file():
        logger.debug("MA wrapper 不存在：%s", wrapper)
        return {}

    init_msg = json.dumps(
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "hermes-ma-plugin", "version": "1.0.0"},
            },
        }
    )
    notified_msg = json.dumps(
        {
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {},
        }
    )
    tool_msg = json.dumps(
        {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": tool_name,
                "arguments": arguments,
            },
        }
    )

    env = {**os.environ, "MCP_MEMORY_PROJECT_ID": "memra"}

    try:
        proc = subprocess.Popen(
            ["bash", wrapper],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=env,
        )
    except Exception as e:
        logger.debug("MA wrapper spawn 失败: %s", e)
        return {}

    assert proc.stdin is not None and proc.stdout is not None  # PIPE 保证非 None

    result_dict: dict = {}
    deadline = time.monotonic() + timeout

    try:
        # 1. 写 initialize
        proc.stdin.write(init_msg + "\n")
        proc.stdin.flush()

        # 2. 等待 id=1 响应（服务 ready 信号）
        init_ok = False
        while time.monotonic() < deadline:
            line = proc.stdout.readline()
            if not line:
                break
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
                if obj.get("id") == 1 and "result" in obj:
                    init_ok = True
                    break
            except json.JSONDecodeError:
                continue

        if not init_ok:
            logger.debug("MA MCP 握手失败（tool=%s）：未收到 initialize 响应", tool_name)
            return {}

        # 3. 写 notifications/initialized + tools/call
        proc.stdin.write(notified_msg + "\n")
        proc.stdin.write(tool_msg + "\n")
        proc.stdin.flush()

        # 4. 逐行读 stdout，找 id=2 响应
        while time.monotonic() < deadline:
            line = proc.stdout.readline()
            if not line:
                break
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
                if obj.get("id") == 2:
                    if "result" in obj:
                        result_dict = obj["result"]
                    elif "error" in obj:
                        logger.debug("MA MCP 工具错误（tool=%s）: %s", tool_name, obj["error"])
                    break
            except json.JSONDecodeError:
                continue

        if not result_dict and time.monotonic() >= deadline:
            logger.warning("MA MCP 调用超时（tool=%s, timeout=%ss）", tool_name, timeout)

    except Exception as e:
        logger.debug("MA MCP 调用异常（tool=%s）: %s", tool_name, e)
    finally:
        # 关闭 stdin 触发进程 EOF 退出，然后终止进程
        try:
            if proc.stdin is not None:
                proc.stdin.close()
        except Exception:
            pass
        try:
            proc.kill()
        except Exception:
            pass
        try:
            proc.wait(timeout=2)
        except Exception:
            pass

    return result_dict


def _extract_text_content(result: dict) -> str:
    """从 MCP tools/call 响应中提取文本内容。

    MCP 响应格式：{"content": [{"type": "text", "text": "..."}]}
    """
    content_list = result.get("content", [])
    parts = []
    for item in content_list:
        if isinstance(item, dict) and item.get("type") == "text":
            text = item.get("text", "")
            if text:
                parts.append(text)
    return "\n".join(parts)


# ---------------------------------------------------------------------------
# MAProvider
# ---------------------------------------------------------------------------


class MAProvider(_HermesBase):  # type: ignore[misc,valid-type]
    """Memra 的 Hermes MemoryProvider 实现（subprocess 方案）。

    继承 MemoryProvider ABC（在 Hermes 环境）或 object（在 MA 仓库测试）。
    实现全部必要方法 + 4 个可选 hook。
    """

    @property
    def name(self) -> str:
        return "memra"

    def __init__(self) -> None:
        self._session_id: str = ""
        self._agent_context: str = "primary"
        # sync_turn 后台写入队列（daemon thread 消费）
        self._write_queue: queue.Queue = queue.Queue(maxsize=_WRITE_QUEUE_MAXSIZE)
        self._writer_thread: Optional[threading.Thread] = None
        self._shutdown_flag = threading.Event()
        # 预取缓存（prefetch 后台填充，prefetch() 返回缓存）
        self._prefetch_cache: str = ""
        self._prefetch_lock = threading.Lock()

    def is_available(self) -> bool:
        """检测 MA wrapper 路径存在且 MCP 握手通。

        不发起完整工具调用，仅做路径 + 进程启动检测。
        """
        wrapper = Path(_MA_WRAPPER_PATH)
        if not wrapper.is_file():
            logger.debug("MA wrapper 不存在：%s", _MA_WRAPPER_PATH)
            return False

        # 发起最小握手：只做 initialize，不调用工具
        # 注意：必须用 Popen 逐行读（MA 是 stdio 服务，stdin 关闭才退出，
        # subprocess.run 写完 stdin 就 EOF，但响应在 stdin close 前可能没到）
        handshake_msg = json.dumps(
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "hermes-ma-probe", "version": "1.0.0"},
                },
            }
        )
        proc: Optional[subprocess.Popen[str]] = None
        try:
            proc = subprocess.Popen(
                ["bash", str(wrapper)],
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                env={**os.environ, "MCP_MEMORY_PROJECT_ID": "memra"},
            )
            assert proc.stdin is not None and proc.stdout is not None  # PIPE 保证非 None
            proc.stdin.write(handshake_msg + "\n")
            proc.stdin.flush()

            deadline = time.monotonic() + 8
            found = False
            while time.monotonic() < deadline:
                line = proc.stdout.readline()
                if not line:
                    break
                try:
                    obj = json.loads(line.strip())
                    if obj.get("id") == 1 and "result" in obj:
                        found = True
                        break
                except json.JSONDecodeError:
                    continue

            return found
        except Exception as e:
            logger.debug("MA handshake 失败: %s", e)
            return False
        finally:
            if proc is not None:
                try:
                    if proc.stdin is not None:
                        proc.stdin.close()
                except Exception:
                    pass
                try:
                    proc.kill()
                except Exception:
                    pass
                try:
                    proc.wait(timeout=2)
                except Exception:
                    pass

    def initialize(self, session_id: str, **kwargs) -> None:
        """会话初始化：记录 session_id + 启动后台写入线程。

        agent_context 为 non-primary（subagent/cron/flush）时跳过写操作，
        避免污染主会话的记忆。
        """
        self._session_id = session_id
        self._agent_context = kwargs.get("agent_context", "primary")

        # 启动后台 daemon thread 处理 sync_turn 写入队列
        self._shutdown_flag.clear()
        self._writer_thread = threading.Thread(
            target=self._write_worker,
            name="ma-write-worker",
            daemon=True,  # daemon=True：进程退出时自动结束，不阻塞 Hermes
        )
        self._writer_thread.start()
        logger.info(
            "MAProvider 初始化完成（session=%s, context=%s）", session_id, self._agent_context
        )

    def system_prompt_block(self) -> str:
        """调 MA get_context(mode=wake) 获取记忆快照注入系统提示。

        返回不超过 _MAX_SYSTEM_PROMPT_CHARS 字符的文本块。
        失败时返回空字符串（graceful degradation）。
        """
        result = _mcp_call("get_context", {"mode": "wake"})
        if not result:
            return ""
        text = _extract_text_content(result)
        if not text:
            return ""
        # 超长截断（保留头部最关键的记忆）
        if len(text) > _MAX_SYSTEM_PROMPT_CHARS:
            text = text[:_MAX_SYSTEM_PROMPT_CHARS] + "\n...[截断]"
        return f"## Memra 记忆快照\n\n{text}"

    def prefetch(self, query: str, *, session_id: str = "") -> str:
        """返回预取缓存的记忆上下文（由 queue_prefetch 后台填充）。"""
        del query, session_id  # ABC 签名占位；prefetch 仅消费缓存，不再查询
        with self._prefetch_lock:
            cached = self._prefetch_cache
            self._prefetch_cache = ""  # 消费后清空，等下一次 queue_prefetch 填充
        return cached

    def queue_prefetch(self, query: str, *, session_id: str = "") -> None:
        """后台预取：提交 search_rules 任务到 daemon thread，下一 turn 由 prefetch() 消费。"""
        del session_id  # ABC 签名占位；MA 端不区分 session_id
        if not query or not query.strip():
            return

        def _do_prefetch(q: str) -> None:
            result = _mcp_call("search_rules", {"query": q, "limit": 3, "min_score": 0.3})
            text = _extract_text_content(result)
            if text:
                with self._prefetch_lock:
                    self._prefetch_cache = text

        t = threading.Thread(target=_do_prefetch, args=(query,), daemon=True)
        t.start()

    def sync_turn(self, user_content: str, assistant_content: str, *, session_id: str = "") -> None:
        """非阻塞地将对话 turn 入队，由后台 daemon thread 写入 MA。

        ABC 明确要求 sync_turn 必须 non-blocking，所以用队列 + daemon thread。
        只在 primary context 下写入（避免 subagent/cron 污染主记忆）。
        """
        del session_id  # ABC 签名占位；MA 端通过 project_id 隔离会话
        if self._agent_context != "primary":
            return
        # 队列满时丢弃（不阻塞主线程）
        try:
            self._write_queue.put_nowait({"user": user_content, "assistant": assistant_content})
        except queue.Full:
            logger.debug("sync_turn 写入队列已满，丢弃此 turn")

    def get_tool_schemas(self) -> List[Dict[str, Any]]:
        """暴露 5 个核心 MA 工具供 Hermes 模型调用。"""
        return [
            {
                "name": "ma_search",
                "description": (
                    "搜索 Memra 记忆库。"
                    "在回答历史/决策/Bug/配置相关问题前，必须先调用此工具。"
                    "支持语义+关键词混合搜索，涵盖 L2 事件记录和 L3 事实记忆。"
                ),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "搜索查询（自然语言）"},
                        "limit": {
                            "type": "integer",
                            "default": 5,
                            "description": "返回结果数（1-20）",
                        },
                        "min_score": {
                            "type": "number",
                            "default": 0.2,
                            "description": "相关度阈值（0-1，降低可提高召回率）",
                        },
                    },
                    "required": ["query"],
                },
            },
            {
                "name": "ma_remember",
                "description": (
                    "向 Memra 写入一条记忆规则/事实。"
                    "用于存储：架构决策、Bug 修复、技术选型、用户偏好、成功/失败模式。"
                    "写入后跨会话永久保留。"
                ),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "content": {"type": "string", "description": "记忆内容（支持 Markdown）"},
                        "category": {
                            "type": "string",
                            "description": "分类（decision/bug/preference/architecture/routine）",
                        },
                    },
                    "required": ["content"],
                },
            },
            {
                "name": "ma_context",
                "description": (
                    "获取 Memra 上下文快照。"
                    "mode=wake：会话启动时拉取完整记忆快照。"
                    "mode=compact：压缩前萃取关键上下文。"
                ),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "mode": {
                            "type": "string",
                            "enum": ["wake", "compact"],
                            "default": "wake",
                            "description": "快照模式",
                        }
                    },
                    "required": [],
                },
            },
            {
                "name": "ma_checkpoint",
                "description": (
                    "保存任务断点到 Memra。"
                    "任务开始：status=in_progress；中断：status=blocked；完成：status=completed。"
                    "同 task_id 的旧断点自动停用（UPSERT 语义）。"
                ),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "task_id": {"type": "string", "description": "任务唯一 ID"},
                        "summary": {"type": "string", "description": "任务摘要和当前状态"},
                        "task_status": {
                            "type": "string",
                            "enum": ["in_progress", "blocked", "completed"],
                        },
                        "blocker": {
                            "type": "string",
                            "description": "阻塞原因（status=blocked 时填写）",
                        },
                    },
                    "required": ["task_id", "summary", "task_status"],
                },
            },
            {
                "name": "ma_find_checkpoints",
                "description": "查找 Memra 中的任务断点（未完成的任务）。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "task_status": {
                            "type": "string",
                            "enum": ["in_progress", "blocked", "completed"],
                            "description": "筛选状态（不填则返回所有）",
                        },
                        "limit": {
                            "type": "integer",
                            "default": 10,
                            "description": "返回数量上限",
                        },
                    },
                    "required": [],
                },
            },
        ]

    def handle_tool_call(self, tool_name: str, args: Dict[str, Any], **kwargs) -> str:
        """将 Hermes 工具调用转发到 MA MCP 工具。

        工具名映射（Hermes 前端名 → MA 原生工具名）：
          ma_search          → search_rules
          ma_remember        → add_rule
          ma_context         → get_context
          ma_checkpoint      → save_checkpoint
          ma_find_checkpoints → search_checkpoints
        """
        del kwargs  # ABC 签名兼容；当前实现不消费额外 kwargs
        tool_map = {
            "ma_search": ("search_rules", args),
            "ma_remember": ("add_rule", args),
            "ma_context": ("get_context", args),
            "ma_checkpoint": ("save_checkpoint", args),
            "ma_find_checkpoints": ("search_checkpoints", args),
        }

        if tool_name not in tool_map:
            return json.dumps({"error": f"未知工具：{tool_name}"})

        ma_tool, ma_args = tool_map[tool_name]
        result = _mcp_call(ma_tool, ma_args)
        if not result:
            return json.dumps({"error": f"MA 工具调用失败：{ma_tool}"})
        text = _extract_text_content(result)
        return json.dumps({"result": text}) if text else json.dumps(result)

    def shutdown(self) -> None:
        """优雅关闭：停止后台写入线程。"""
        self._shutdown_flag.set()
        # 投入哨兵值唤醒 worker 退出
        try:
            self._write_queue.put_nowait(None)
        except queue.Full:
            pass
        if self._writer_thread and self._writer_thread.is_alive():
            self._writer_thread.join(timeout=3)

    # -- 可选 hook 实现 -------------------------------------------------------

    def on_pre_compress(self, messages: List[Dict[str, Any]]) -> str:
        """压缩前调 MA get_context(mode=compact) 萃取关键上下文。

        返回文本供压缩摘要提示词使用，防止重要记忆在压缩中丢失。
        """
        del messages  # ABC 签名占位；MA 端独立维护快照，不需要 hermes messages
        result = _mcp_call("get_context", {"mode": "compact"}, timeout=10)
        if not result:
            return ""
        text = _extract_text_content(result)
        return f"## Memra 压缩前上下文\n\n{text}" if text else ""

    def on_session_end(self, messages: List[Dict[str, Any]]) -> None:
        """会话结束时，扫描对话提取关键决策/事实写入 MA。

        只处理 primary context（非 subagent/cron），避免污染。
        只处理 assistant 消息中包含关键标记的内容（决策/修复/配置）。
        """
        if self._agent_context != "primary":
            return
        if not messages:
            return

        # 提取 assistant 消息中包含决策性内容的段落
        _DECISION_MARKERS = (
            "[DECISION]",
            "[CORRECTION]",
            "[SUCCESS]",
            "[FAILURE]",
            "决定",
            "选择了",
            "修复了",
            "配置了",
            "发现了",
        )
        extracted = []
        for msg in messages:
            if msg.get("role") != "assistant":
                continue
            content = msg.get("content", "")
            if not isinstance(content, str) or len(content) < 20:
                continue
            # 只摘取包含决策标记的消息（避免把普通回复全写进 MA）
            if any(marker in content for marker in _DECISION_MARKERS):
                extracted.append(content[:600])

        for snippet in extracted[:3]:  # 每次会话最多写 3 条，防止 MA 膨胀
            try:
                _mcp_call(
                    "add_rule",
                    {"content": f"[会话萃取] {snippet}", "category": "decision"},
                    timeout=12,
                )
            except Exception as e:
                logger.debug("on_session_end 写入失败: %s", e)

    def on_memory_write(
        self,
        action: str,
        target: str,
        content: str,
        metadata: Optional[Dict[str, Any]] = None,
    ) -> None:
        """镜像 Hermes 内置记忆写入到 MA（仅 add 操作）。

        让 MA 同步感知 MEMORY.md / USER.md 的写入，实现双轨记忆同步。
        """
        del metadata  # ABC 签名占位；当前不传到 add_rule（未来可能用）
        if action != "add" or not content:
            return
        if self._agent_context != "primary":
            return
        # 非阻塞投入队列
        payload = {
            "user": f"[内置记忆写入] target={target}\n{content}",
            "assistant": "",
        }
        try:
            self._write_queue.put_nowait(payload)
        except queue.Full:
            pass

    # -- 后台写入 worker -------------------------------------------------------

    def _write_worker(self) -> None:
        """后台 daemon thread：消费 _write_queue，将 turn 摘要写入 MA。

        只写 assistant 内容中含关键标记的 turn，避免把每条对话都塞进 MA。
        """
        _WRITE_MARKERS = (
            "[DECISION]",
            "[CORRECTION]",
            "[SUCCESS]",
            "[FAILURE]",
            "add_rule",
            "save_checkpoint",
        )
        while not self._shutdown_flag.is_set():
            try:
                item = self._write_queue.get(timeout=1)
            except queue.Empty:
                continue

            if item is None:  # 哨兵值，退出
                break

            assistant_text = item.get("assistant", "")
            # 仅当 assistant 回复中有关键标记时才写入，避免噪音
            if not any(marker in assistant_text for marker in _WRITE_MARKERS):
                self._write_queue.task_done()
                continue

            summary = assistant_text[:400]
            try:
                _mcp_call(
                    "add_rule",
                    {"content": f"[turn 自动记录] {summary}", "category": "decision"},
                    timeout=10,
                )
            except Exception as e:
                logger.debug("后台写入失败: %s", e)
            finally:
                self._write_queue.task_done()


# 基类已在文件顶部 import 时决定，class MAProvider(_HermesBase) 静态继承
# （旧版本用 __bases__ 替换会触发 "deallocator differs" 警告，已移除）
