"""ma:doctor — Memra 连接状态检测。

转发到 ./memra doctor，并附加 Hermes plugin 层的状态检测。
"""

from __future__ import annotations

import subprocess
from pathlib import Path

_WRAPPER = Path.home() / "projects" / "memra" / "scripts" / "mcp_wrapper.sh"


def run_doctor() -> None:
    """运行 MA doctor 并输出 plugin 层状态。"""
    print("\n=== Memra 状态检测 ===\n")

    # 检测 wrapper 是否存在
    if not _WRAPPER.is_file():
        print(f"✗ MA wrapper 不存在：{_WRAPPER}")
        print("  请运行 /ma:setup 完成安装。")
        return
    print(f"✓ MA wrapper 存在：{_WRAPPER}")

    # 运行 MA 自带的 doctor
    ma_cli = _WRAPPER.parent.parent / "ma"
    if ma_cli.is_file():
        print("\n运行 ./memra doctor...\n")
        try:
            result = subprocess.run(
                [str(ma_cli), "doctor", "--project", "memra"],
                capture_output=False,
                timeout=30,
            )
            if result.returncode != 0:
                print(f"\n✗ memra doctor 退出码：{result.returncode}")
        except subprocess.TimeoutExpired:
            print("✗ memra doctor 超时（30s）")
        except Exception as e:
            print(f"✗ 运行 memra doctor 失败：{e}")
    else:
        print("  （未找到 memra CLI，跳过 doctor 命令）")

    # plugin 自检：MCP 握手测试
    print("\n测试 MCP 握手...")
    from ..memory_provider import MAProvider

    provider = MAProvider()
    if provider.is_available():
        print("✓ MCP 握手成功，MA 服务正常")
    else:
        print("✗ MCP 握手失败")
        print("  请检查：MA 是否正在运行（./ma up）？wrapper 路径是否正确？")


if __name__ == "__main__":
    run_doctor()
