"""Memra — Hermes native memory provider 插件。

注册方式：Hermes plugin 系统在加载插件时调用 register(ctx)。
ctx.register_memory_provider(provider) 将 MAProvider 注入 MemoryManager。

安装：将此目录 symlink 到 ~/.hermes/hermes-agent/plugins/memory/memra
然后在 ~/.hermes/config.yaml 中配置：
  memory:
    provider: memra
"""

from __future__ import annotations


def register(ctx) -> None:
    """Hermes plugin 入口：注册 MAProvider 到 MemoryManager。

    Hermes 用 importlib.util.spec_from_file_location 把本包注册为
    `_hermes_user_memory.memra`，并预注册子模块到 sys.modules。
    必须用相对 import（`from .memory_provider`），不能用顶层 absolute
    import（`from memory_provider`），后者会 ModuleNotFoundError。
    """
    from .memory_provider import MAProvider  # noqa: PLC0415

    provider = MAProvider()
    ctx.register_memory_provider(provider)
