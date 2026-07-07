# Memra — Claude Code Plugin Installation

## 快速安装（双机通用）

### 一次性 symlink (推荐)

```bash
# 1. 备份当前 plugin（防回退）
mv ~/.claude/plugins/local-plugins/memra \
   ~/.claude/plugins/local-plugins/memra.bak.$(date +%Y%m%d)

# 2. symlink 到 repo（git pull 自动同步）
ln -sf ~/projects/memra \
       ~/.claude/plugins/local-plugins/memra

# 3. 重启 Claude Code 让 plugin manager 重新扫描
```

### ⚠️ 必须做的 Migration（避免 hook double-fire）

Plugin 内置 lifecycle hooks（SessionStart / PreCompact / PostCompact）。
**安装前必须从 `~/.claude/settings.json` 删除现有 MA hook 行**，否则会触发两次。

需要删除的旧行（搜 `~/.claude/settings.json` 找）：

```jsonc
// SessionStart 段下
{ "command": ".../ma-pending-nudge.py" }

// PreCompact 段下
{ "command": ".../pre-compact-checkpoint.py" }

// PostCompact 段下（如果有）
{ "command": ".../post-compact-reinject.py" }
```

> 不删的后果：每次 SessionStart / PreCompact / PostCompact 都触发两次，写双倍断点 + nudge。

### 验证

```bash
# 1. 确认 plugin 被发现
ls -la ~/.claude/plugins/local-plugins/memra
# 应该是 symlink → ~/projects/memra

# 2. 确认 plugin.json 版本
cat ~/.claude/plugins/local-plugins/memra/.claude-plugin/plugin.json | grep version
# "version": "7.0.0"

# 3. 重启 Claude Code，新 session 应自动加载 MA MCP server + skills

# 4. 测一次 PreCompact 不会重复触发
# 触发 /compact，看 ~/.memra/projects/memra/compaction-flush/
# 应该只多 1 个 transcript 文件，不是 2 个
```

## 回滚

```bash
# 删 symlink，恢复备份
rm ~/.claude/plugins/local-plugins/memra
mv ~/.claude/plugins/local-plugins/memra.bak.YYYYMMDD \
   ~/.claude/plugins/local-plugins/memra
# 在 ~/.claude/settings.json 把 MA hook 行加回去
```

## Mac Mini 同步

```bash
# Mac Mini 上
cd ~/projects/memra && git pull origin main
# 重复上面"一次性 symlink"步骤（Mac Mini 第一次装时）
# 之后只需 git pull，plugin 自动更新（symlink 指向 repo）
```

## Plugin 包含什么

```
.claude-plugin/plugin.json   # 元数据 (v7.0.0)
.mcp.json                    # MCP server 配置（指向 scripts/mcp_wrapper.sh）
commands/                    # /mem, /remember 两个 slash command
  mem.md
  remember.md
agents/                      # memory-curator subagent
  memory-curator.md
skills/                      # memory-rules skill
  memory-rules/
hooks/                       # lifecycle hooks
  hooks.json
  ma-hook-rust.sh            # SessionStart / PreCompact / PostCompact / add_rule validator
```

## 已知 TODO（follow-up）

### Closed (2026-05-08 night, audit-driven second-round siege)
- [x] commands/mem.md 删 Qdrant 引用 (commit e7fa8f9)
- [x] agents/memory-curator.md 8 → 16 工具 (commit f3ced20)
- [x] CLAUDE.md "MCP Tools (8)" → "(16)" 同步 (this commit)
- [x] commands/remember.md v3 工具名 → v6 (search_memory→search_rules etc.) (this commit)
- [x] skills/memory-rules/SKILL.md v3 工具名 + Rust MCP 工具面同步 (this commit)
- [x] hooks/_shared.py IMPLICIT_RECALL_SCRIPT 加 ${CLAUDE_PLUGIN_ROOT} fallback (archived in R4)
- [x] docs/runbook/mbp-as-primary-ma-host.md 删假 `memra search` 命令 (this commit)
- [x] hooks/hooks.json 已切到 `hooks/ma-hook-rust.sh`，R4 当前树不再发布旧 hook 脚本

### Still open
- [ ] 测试 ${CLAUDE_PLUGIN_ROOT} 在真 plugin manager context 是否正确解析（要 Claude Code 重启后真触发 hook 才知）
- [ ] Mac mini settings.json 同样删 3 个老 hook（防 git pull 后双触发，要 ssh mac-mini-ts 单独处理）
- [ ] Gemma 替代 autoresearch 的 Gemini/Codex（见 docs/proposals/gemma-autoresearch-llm.md，DRAFT）
- [ ] MBP-as-primary-MA-host 主机切换执行（见 docs/runbook/mbp-as-primary-ma-host.md，等 5/9 后窗口）
