"""ma:setup — Memra 配置向导。

交互步骤：
  1. 检测 MA 仓库路径（~/.projects/memra 或 ~/projects/memra）
  2. 选择接入模式（native plugin / MCP-only）
  3. 确认 symlink 到 Hermes plugins 目录

在 Hermes 命令系统中注册为 /ma:setup。
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

# Hermes 默认搜索路径列表（按优先级排序）
_MA_SEARCH_PATHS = [
    Path.home() / "projects" / "memra",
    Path.home() / ".memra" / "src",
    Path("/opt/memra"),
]

_HERMES_PLUGIN_DIR_REL = "hermes-agent/plugins/memory"


def _find_ma_repo() -> Path | None:
    """自动检测 MA 仓库路径。"""
    # 先查环境变量
    env_path = os.environ.get("MA_REPO_PATH")
    if env_path:
        p = Path(env_path)
        if (p / "scripts" / "mcp_wrapper.sh").is_file():
            return p

    for candidate in _MA_SEARCH_PATHS:
        if (candidate / "scripts" / "mcp_wrapper.sh").is_file():
            return candidate
    return None


def _find_hermes_home() -> Path | None:
    """定位 HERMES_HOME 目录。"""
    env_home = os.environ.get("HERMES_HOME")
    if env_home:
        p = Path(env_home)
        if p.is_dir():
            return p
    default = Path.home() / ".hermes"
    if default.is_dir():
        return default
    return None


def run_setup() -> None:
    """交互式安装向导（3 步）。"""
    print("\n=== Memra × Hermes 安装向导 ===\n")

    # 步骤 1：检测 MA 仓库
    print("步骤 1/3：检测 Memra 仓库...")
    ma_repo = _find_ma_repo()
    if ma_repo:
        print(f"  ✓ 找到 MA 仓库：{ma_repo}")
    else:
        manual = input("  ✗ 未找到。请输入 MA 仓库路径（含 scripts/mcp_wrapper.sh）：").strip()
        if not manual:
            print("  ✗ 未输入路径，退出。")
            sys.exit(1)
        ma_repo = Path(manual)
        if not (ma_repo / "scripts" / "mcp_wrapper.sh").is_file():
            print(f"  ✗ {ma_repo}/scripts/mcp_wrapper.sh 不存在，退出。")
            sys.exit(1)
        print(f"  ✓ 使用：{ma_repo}")

    # 步骤 2：选择接入模式
    print("\n步骤 2/3：选择接入模式")
    print("  [1] native plugin（推荐）— 启用 lifecycle hooks + system prompt 注入")
    print("  [2] MCP-only — 保持现有 MCP 接入（不安装 plugin）")
    choice = input("  请选择 [1/2，默认 1]：").strip() or "1"
    if choice == "2":
        print("\n已选择 MCP-only 模式。native plugin 不安装。")
        print("如需将来切换，重新运行 /ma:setup。")
        return

    # 步骤 3：确认 symlink
    print("\n步骤 3/3：创建 symlink 到 Hermes plugins 目录")
    hermes_home = _find_hermes_home()
    if not hermes_home:
        hermes_home_str = input("  未找到 HERMES_HOME。请输入路径：").strip()
        if not hermes_home_str:
            print("  ✗ 未输入路径，退出。")
            sys.exit(1)
        hermes_home = Path(hermes_home_str)

    plugin_dir = hermes_home / _HERMES_PLUGIN_DIR_REL
    target = plugin_dir / "memra"
    source = ma_repo / "plugins" / "memra"

    print(f"  symlink: {target} → {source}")
    confirm = input("  确认创建？[y/N]：").strip().lower()
    if confirm != "y":
        print("  取消。可手动执行：")
        print(f"    ln -sf {source} {target}")
        return

    plugin_dir.mkdir(parents=True, exist_ok=True)
    if target.exists() or target.is_symlink():
        target.unlink()
    target.symlink_to(source)
    print(f"  ✓ symlink 创建成功：{target}")

    # 提示下一步
    print("\n安装完成！下一步：")
    print("  1. 编辑 ~/.hermes/config.yaml，添加：")
    print("     memory:")
    print("       provider: memra")
    print("  2. 重启 Hermes：hermes 或 hermes --profile <your-profile>")
    print("  3. 运行 /ma:doctor 验证连接状态\n")


if __name__ == "__main__":
    run_setup()
