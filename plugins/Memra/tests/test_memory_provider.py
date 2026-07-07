"""MAProvider 测试套件（5 个用例，含 1 个真实 MCP 握手集成测试）。

测试策略：
  - 单元测试用 mock 替代 subprocess，不依赖 MA 服务运行
  - 集成测试（test_real_handshake）真实 spawn MA wrapper，验证 MCP 协议握手
  - 不 mock 整个 MAProvider，每个测试都实例化真实 MAProvider

运行方式：
  uv run pytest plugins/memra/tests/ -x -v
"""

from __future__ import annotations

import json
import sys
import time
import unittest
from pathlib import Path
from unittest.mock import patch

# 将插件目录加入 sys.path，直接从插件包目录 import（目录名含连字符，不能用包路径）
_PLUGIN_DIR = Path(__file__).parent.parent  # .../plugins/memra/
if str(_PLUGIN_DIR) not in sys.path:
    sys.path.insert(0, str(_PLUGIN_DIR))

from memory_provider import (  # noqa: E402
    _MA_WRAPPER_PATH,
    MAProvider,
    _extract_text_content,
    _mcp_call,
)

# --------------------------------------------------------------------------- #
# 测试用例 1：is_available() 真假分支
# --------------------------------------------------------------------------- #


class TestIsAvailable(unittest.TestCase):
    """验证 is_available() 的两种路径。"""

    def test_returns_false_when_wrapper_missing(self):
        """wrapper 路径不存在时应返回 False。"""
        provider = MAProvider()
        with patch(
            "memory_provider._MA_WRAPPER_PATH",
            "/nonexistent/path/mcp_wrapper.sh",
        ):
            # 直接 patch Path.is_file 也可以，但 patch 常量更直接
            result = provider.is_available()
        # 路径不存在必然 False
        self.assertFalse(result)

    def test_returns_false_when_subprocess_fails(self):
        """subprocess.Popen 失败（OSError/异常）时 is_available() 应返回 False，不抛异常。"""
        provider = MAProvider()
        with patch("subprocess.Popen", side_effect=OSError("spawn 失败")):
            with patch(
                "memory_provider.Path.is_file",
                return_value=True,
            ):
                result = provider.is_available()
        self.assertFalse(result)


# --------------------------------------------------------------------------- #
# 测试用例 2：get_tool_schemas() 结构验证
# --------------------------------------------------------------------------- #


class TestGetToolSchemas(unittest.TestCase):
    """验证 get_tool_schemas() 返回至少 5 个合规 schema。"""

    def setUp(self):
        self.provider = MAProvider()

    def test_returns_at_least_five_schemas(self):
        schemas = self.provider.get_tool_schemas()
        self.assertGreaterEqual(len(schemas), 5, "至少暴露 5 个 MA 核心工具")

    def test_each_schema_has_required_fields(self):
        schemas = self.provider.get_tool_schemas()
        for schema in schemas:
            self.assertIn("name", schema, f"schema 缺少 name: {schema}")
            self.assertIn("description", schema, f"schema 缺少 description: {schema}")
            self.assertIn("parameters", schema, f"schema 缺少 parameters: {schema}")
            self.assertIsInstance(schema["name"], str)
            self.assertGreater(len(schema["name"]), 0)

    def test_core_tools_present(self):
        schemas = self.provider.get_tool_schemas()
        names = {s["name"] for s in schemas}
        required = {
            "ma_search",
            "ma_remember",
            "ma_context",
            "ma_checkpoint",
            "ma_find_checkpoints",
        }
        missing = required - names
        self.assertFalse(missing, f"缺少必要工具：{missing}")


# --------------------------------------------------------------------------- #
# 测试用例 3：handle_tool_call() mock 转发验证
# --------------------------------------------------------------------------- #


class TestHandleToolCall(unittest.TestCase):
    """验证工具调用转发到正确的 MA 工具名。"""

    def setUp(self):
        self.provider = MAProvider()

    def _make_mcp_response(self, text: str) -> dict:
        """构造符合 MCP 协议的 tools/call 响应。"""
        return {"content": [{"type": "text", "text": text}]}

    def test_ma_search_routes_to_search_rules(self):
        """ma_search 应转发到 search_rules。"""
        mock_result = self._make_mcp_response("找到 3 条记忆")
        with patch(
            "memory_provider._mcp_call",
            return_value=mock_result,
        ) as mock_call:
            result = self.provider.handle_tool_call(
                "ma_search", {"query": "MA 架构决策", "limit": 3}
            )
        mock_call.assert_called_once_with("search_rules", {"query": "MA 架构决策", "limit": 3})
        parsed = json.loads(result)
        self.assertIn("result", parsed)

    def test_ma_remember_routes_to_add_rule(self):
        """ma_remember 应转发到 add_rule。"""
        mock_result = self._make_mcp_response('{"id": "abc123", "status": "ok"}')
        with patch(
            "memory_provider._mcp_call",
            return_value=mock_result,
        ) as mock_call:
            result = self.provider.handle_tool_call(
                "ma_remember", {"content": "测试记忆", "category": "decision"}
            )
        mock_call.assert_called_once_with(
            "add_rule", {"content": "测试记忆", "category": "decision"}
        )
        self.assertIsNotNone(result)  # 转发后应返回非 None 响应

    def test_unknown_tool_returns_error_json(self):
        """未知工具名应返回 JSON 格式的错误，不抛异常。"""
        result = self.provider.handle_tool_call("nonexistent_tool", {})
        parsed = json.loads(result)
        self.assertIn("error", parsed)

    def test_mcp_failure_returns_error_json(self):
        """_mcp_call 返回空 dict 时应返回 JSON 错误。"""
        with patch("memory_provider._mcp_call", return_value={}):
            result = self.provider.handle_tool_call("ma_search", {"query": "test"})
        parsed = json.loads(result)
        self.assertIn("error", parsed)


# --------------------------------------------------------------------------- #
# 测试用例 4：system_prompt_block() 不超长
# --------------------------------------------------------------------------- #


class TestSystemPromptBlock(unittest.TestCase):
    """验证 system_prompt_block() 截断逻辑正确。"""

    def setUp(self):
        self.provider = MAProvider()

    def test_truncates_at_max_chars(self):
        """超长内容应被截断到 _MAX_SYSTEM_PROMPT_CHARS 以内（允许追加截断标记）。"""
        from memory_provider import _MAX_SYSTEM_PROMPT_CHARS

        long_text = "记忆条目。" * 1000  # ~5000 字符
        mock_result = {"content": [{"type": "text", "text": long_text}]}
        with patch("memory_provider._mcp_call", return_value=mock_result):
            block = self.provider.system_prompt_block()
        # 包含截断标记时稍长于阈值，但正文部分不超限
        self.assertLessEqual(
            len(block),
            _MAX_SYSTEM_PROMPT_CHARS + 200,  # 200 字符给标题和截断标记
            "system_prompt_block() 内容应受最大字符数限制",
        )

    def test_returns_empty_on_mcp_failure(self):
        """MA 服务不可用时应返回空字符串，不崩溃。"""
        with patch("memory_provider._mcp_call", return_value={}):
            block = self.provider.system_prompt_block()
        self.assertEqual(block, "")

    def test_returns_string(self):
        """返回类型必须是 str。"""
        mock_result = {"content": [{"type": "text", "text": "记忆内容"}]}
        with patch("memory_provider._mcp_call", return_value=mock_result):
            block = self.provider.system_prompt_block()
        self.assertIsInstance(block, str)


# --------------------------------------------------------------------------- #
# 测试用例 5：sync_turn() 使用 daemon thread（非阻塞）
# --------------------------------------------------------------------------- #


class TestSyncTurnNonBlocking(unittest.TestCase):
    """验证 sync_turn() 真正用了 daemon thread，不阻塞调用方。"""

    def test_sync_turn_uses_daemon_thread(self):
        """初始化后，_writer_thread 应是 daemon=True 的线程。"""
        provider = MAProvider()
        provider.initialize("test-session-001", agent_context="primary")
        self.assertIsNotNone(provider._writer_thread)
        assert provider._writer_thread is not None  # narrow type for pyright
        self.assertTrue(
            provider._writer_thread.daemon,
            "_writer_thread 必须是 daemon thread（防止阻塞进程退出）",
        )
        provider.shutdown()

    def test_sync_turn_is_nonblocking(self):
        """sync_turn() 调用本身应在 50ms 内返回（写操作在后台线程处理）。"""
        provider = MAProvider()
        provider.initialize("test-session-002", agent_context="primary")

        start = time.monotonic()
        # 模拟 10 个 turn 的连续写入
        for i in range(10):
            provider.sync_turn(f"用户消息 {i}", f"[DECISION] 助手决策 {i}")
        elapsed = time.monotonic() - start

        self.assertLess(
            elapsed,
            0.5,  # 10 次入队应在 500ms 内完成（实际应 < 1ms）
            f"sync_turn() 不应阻塞（实际耗时 {elapsed:.3f}s）",
        )
        provider.shutdown()

    def test_sync_turn_skips_non_primary_context(self):
        """非 primary context（subagent/cron）时，sync_turn 应跳过写入。"""
        provider = MAProvider()
        provider.initialize("test-session-003", agent_context="subagent")
        # 模拟写入
        provider.sync_turn("用户消息", "[DECISION] 决策")
        # 队列应为空（subagent context 不写）
        self.assertEqual(provider._write_queue.qsize(), 0)
        provider.shutdown()


# --------------------------------------------------------------------------- #
# 集成测试：真实 MCP 握手（test_real_handshake）
# --------------------------------------------------------------------------- #


class TestRealHandshake(unittest.TestCase):
    """真实 spawn MA wrapper，验证 MCP 协议握手。

    依赖 MA 服务可用，带 timeout 防止挂死。
    如果 MA wrapper 不存在则跳过（避免 CI 失败）。
    """

    @classmethod
    def setUpClass(cls):
        """检查 wrapper 是否存在，不存在则跳过整个类。"""
        if not Path(_MA_WRAPPER_PATH).is_file():
            raise unittest.SkipTest(
                f"MA wrapper 不存在（{_MA_WRAPPER_PATH}），跳过集成测试。"
                "请先完成 MA 安装（/ma:setup）。"
            )

    def test_real_handshake_succeeds(self):
        """真实 MCP 握手应在 10 秒内成功。

        验证：spawn wrapper → initialize → 收到 id=1 的成功响应。
        """
        provider = MAProvider()
        # is_available() 内部就是做真实握手
        result = provider.is_available()
        self.assertTrue(
            result,
            "真实 MCP 握手失败。请检查 MA 服务是否正常（./memra doctor --project memra）",
        )

    def test_real_get_context_call(self):
        """真实调用 get_context，验证 MA 工具链路通。

        使用 get_context 而非 search_rules：
        - get_context 不依赖 fastembed 冷启动（fastembed 首次加载 ~10s，超短连接超时）
        - get_context 在 initialize 握手完成后立即可响应
        超时设为 15 秒，足够 get_context 完成。
        """
        result = _mcp_call("get_context", {}, timeout=15)
        # 只要不返回空 dict 就算通（可能上下文为空但协议是通的）
        self.assertIsInstance(
            result,
            dict,
            "MA 工具调用应返回 dict，不应崩溃",
        )
        # content 字段存在（MCP 正常响应的标志）
        self.assertIn(
            "content",
            result,
            f"响应缺少 content 字段，实际：{result}",
        )


# --------------------------------------------------------------------------- #
# 辅助函数测试
# --------------------------------------------------------------------------- #


class TestExtractTextContent(unittest.TestCase):
    """验证 _extract_text_content() 的解析逻辑。"""

    def test_extracts_text_from_mcp_response(self):
        result = {"content": [{"type": "text", "text": "记忆内容"}]}
        self.assertEqual(_extract_text_content(result), "记忆内容")

    def test_handles_empty_content(self):
        self.assertEqual(_extract_text_content({}), "")
        self.assertEqual(_extract_text_content({"content": []}), "")

    def test_joins_multiple_text_blocks(self):
        result = {
            "content": [
                {"type": "text", "text": "第一段"},
                {"type": "text", "text": "第二段"},
            ]
        }
        text = _extract_text_content(result)
        self.assertIn("第一段", text)
        self.assertIn("第二段", text)


if __name__ == "__main__":
    unittest.main(verbosity=2)
