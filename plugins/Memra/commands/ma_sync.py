"""ma:sync — 手动刷新 MA skill bundle。

触发 MA 的 experience-artifacts 生成，更新 Hermes system prompt 注入的内容。
"""

from __future__ import annotations

from ..memory_provider import _extract_text_content, _mcp_call


def run_sync() -> None:
    """手动触发 MA skill bundle 刷新。"""
    print("\n=== Memra Skill Bundle 同步 ===\n")

    print("正在拉取最新上下文快照...")
    result = _mcp_call("get_context", {"mode": "wake"})
    if not result:
        print("✗ 获取上下文失败，请检查 MA 服务状态（/ma:doctor）")
        return

    text = _extract_text_content(result)
    if text:
        preview = text[:300]
        print(f"✓ 上下文快照已刷新（{len(text)} 字符）")
        print(f"\n预览（前 300 字符）:\n{preview}")
        if len(text) > 300:
            print("...")
    else:
        print("✓ 同步完成（上下文为空，MA 记忆库可能为空）")

    print("\n提示：system_prompt_block() 将在下次 Hermes 会话启动时自动注入新快照。")


if __name__ == "__main__":
    run_sync()
