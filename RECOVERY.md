# AISync 项目恢复文档

## 损坏原因分析

### 根因链

1. **send.failed 假阴性**（06-19）：leader 向 worker 发消息报 `send_unverified_exhausted`，但消息实际到达了。leader 误判消息未送达。
2. **leader 误操作 remove-agent --force**：基于假阴性误判，对 reviewer 和 session 执行了 remove-agent，清空了 session_id 绑定。
3. **CLAUDE_CONFIG_DIR 未隔离**：claude_code worker（frontend/session/reviewer）和 leader 共享 `~/.claude/`，worker 接管 leader session 导致 leader 闪退。
4. **codex session 文件不持久**：codex 的 rollout 文件（core/net/qa）在 `/Users/alauda/.codex/sessions/` 下已被系统清理。
5. **restart 时 3/6 角色 fresh 启动**：qa/session/reviewer 的 session_id=None，无法恢复，被强制 fresh start。

### 各角色 transcript 存活状态

| 角色 | Provider | transcript 文件 | 状态 |
|------|----------|----------------|------|
| frontend | claude_code | 7d5f430a.jsonl (44MB) | ✅ 存活但被 leader session 污染 |
| core | codex | rollout-019edbc6.jsonl | ❌ 文件已清理 |
| net | codex | rollout-019edbc7.jsonl | ❌ 文件已清理 |
| qa | codex | 无 | ❌ 从未正确捕获（rollout 指向 Rust build artifact） |
| session | claude_code | 无 | ❌ session_id 被 remove-agent 清空 |
| reviewer | claude_code | 无 | ❌ session_id 被 remove-agent 清空 |

### 可恢复数据源

虽然 transcript 文件大部分丢失，但 **team.db 数据库完整保存了所有交互历史**：

- **862 条消息**（所有角色间的完整通信记录）
- **131 条 results**（所有角色的任务汇报）
- 消息包含完整的 content（任务指令、代码改动、bug 报告、评审意见等）

这是恢复上下文的关键数据源。

---

## 恢复步骤

### 前提
- team-agent 已更新到 0.3.32+（包含全部修复）
- 原项目路径可用：`/Users/alauda/Documents/code/AI对话和进度同步/`
- runtime 数据在原路径：`/Users/alauda/Documents/code/AI对话和进度同步/.team/runtime/`

### 步骤 1：导出每个角色的完整上下文

从 team.db 中提取每个角色收发的所有消息，生成上下文摘要文件。

```bash
cd /Users/alauda/Documents/code/AI对话和进度同步

# 为每个角色导出消息历史
for agent in core net frontend qa session reviewer; do
    echo "=== $agent ===" > /tmp/aisync-recovery-$agent.md
    echo "" >> /tmp/aisync-recovery-$agent.md
    
    echo "### 收到的任务（leader → $agent）" >> /tmp/aisync-recovery-$agent.md
    sqlite3 .team/runtime/team.db "
        SELECT datetime(created_at), content FROM messages 
        WHERE recipient='$agent' AND sender='leader'
        ORDER BY rowid;
    " >> /tmp/aisync-recovery-$agent.md
    
    echo "" >> /tmp/aisync-recovery-$agent.md
    echo "### 汇报结果（$agent → leader）" >> /tmp/aisync-recovery-$agent.md
    sqlite3 .team/runtime/team.db "
        SELECT datetime(created_at), content FROM messages 
        WHERE sender='$agent' AND recipient='leader'
        ORDER BY rowid;
    " >> /tmp/aisync-recovery-$agent.md
    
    echo "" >> /tmp/aisync-recovery-$agent.md
    echo "### 与其他角色的通信" >> /tmp/aisync-recovery-$agent.md
    sqlite3 .team/runtime/team.db "
        SELECT datetime(created_at), sender, recipient, content FROM messages 
        WHERE (sender='$agent' OR recipient='$agent')
        AND sender != 'leader' AND recipient != 'leader'
        AND sender != 'coordinator' AND recipient != 'coordinator'
        ORDER BY rowid;
    " >> /tmp/aisync-recovery-$agent.md
    
    echo "" >> /tmp/aisync-recovery-$agent.md
    echo "### Results" >> /tmp/aisync-recovery-$agent.md
    sqlite3 .team/runtime/team.db "
        SELECT result_id, status, envelope FROM results 
        WHERE agent_id='$agent'
        ORDER BY rowid;
    " >> /tmp/aisync-recovery-$agent.md
    
    echo "导出完成: /tmp/aisync-recovery-$agent.md"
done
```

### 步骤 2：生成综合上下文摘要

```bash
# 导出完整的项目进度时间线
sqlite3 .team/runtime/team.db "
    SELECT datetime(created_at), sender, recipient, substr(content, 1, 200) 
    FROM messages 
    WHERE sender != 'coordinator'
    ORDER BY rowid;
" > /tmp/aisync-recovery-timeline.txt

echo "时间线导出完成: /tmp/aisync-recovery-timeline.txt"
```

### 步骤 3：清理旧的 runtime 状态

```bash
cd /Users/alauda/Documents/code/AI对话和进度同步

# 备份当前 runtime
cp -r .team/runtime/ .team/runtime-backup-$(date +%Y%m%d)/

# 重置 agent 状态（保留 team.db 不动）
# 将所有 agent 的 session_id 和 rollout_path 清空，让 restart 时 fresh start
python3 -c "
import json
with open('.team/runtime/state.json') as f:
    state = json.load(f)
for aid in state.get('agents', {}):
    agent = state['agents'][aid]
    agent['session_id'] = None
    agent['rollout_path'] = None
    agent['status'] = 'stopped'
with open('.team/runtime/state.json', 'w') as f:
    json.dump(state, f, indent=2)
print('State reset done')
"
```

### 步骤 4：重新启动 team

```bash
cd /Users/alauda/Documents/code/AI对话和进度同步

# 启动 leader
team-agent claude

# 在 Claude Code 中，重启 team
# team-agent restart . --allow-fresh
# 或者逐个启动 worker
team-agent start-agent core --allow-fresh
team-agent start-agent net --allow-fresh
team-agent start-agent frontend --allow-fresh
team-agent start-agent qa --allow-fresh
team-agent start-agent session --allow-fresh
team-agent start-agent reviewer --allow-fresh
```

### 步骤 5：为每个 worker 注入历史上下文

对每个 fresh 启动的 worker，发送其历史消息摘要，让它理解自己之前做了什么。

```bash
# 对每个 agent，发送上下文恢复消息
for agent in core net frontend qa session reviewer; do
    team-agent send $agent "【上下文恢复】你是 aisync-dev team 的 $agent 角色。由于系统故障你的上下文丢失了，以下是你之前的完整工作记录。请阅读后回复确认你理解了之前的进度。

$(cat /tmp/aisync-recovery-$agent.md)"
done
```

注意：如果消息过长（超过 tmux paste 限制），需要：
1. 将上下文文件写入工作目录
2. 发送简短消息让 worker 自己读文件

```bash
# 替代方案：写文件让 worker 自己读
for agent in core net frontend qa session reviewer; do
    cp /tmp/aisync-recovery-$agent.md /Users/alauda/Documents/code/AI对话和进度同步/.team/recovery-$agent.md
    team-agent send $agent "【上下文恢复】你的上下文因系统故障丢失。请读取 .team/recovery-$agent.md 了解你之前的完整工作记录，然后回复确认你理解了进度和当前状态。"
done
```

### 步骤 6：验证恢复

```bash
# 检查每个 agent 是否理解了上下文
team-agent status --json | python3 -c "
import json, sys
d = json.load(sys.stdin)
for aid, a in d.get('agents', {}).items():
    sid = a.get('session_id', '-')
    status = a.get('activity', {}).get('status', '-')
    print(f'{aid}: session={sid} activity={status}')
"

# 对每个 agent 发一个简单的确认问题
for agent in core net frontend qa session reviewer; do
    team-agent send $agent "请简短回答：你之前做的最后一件事是什么？当前有什么未完成的工作？"
done
```

---

## 数据统计

### messages 表 (862 条)

| 发送方 → 接收方 | 数量 |
|----------------|------|
| qa → leader | 142 |
| coordinator → leader | 113 |
| leader → frontend | 106 |
| core → leader | 82 |
| leader → qa | 77 |
| frontend → leader | 73 |
| net → leader | 61 |
| leader → net | 40 |
| leader → core | 34 |
| reviewer → leader | 23 |
| core ↔ frontend | 30 |
| leader → reviewer | 9 |
| leader → session | 9 |
| session → leader | 11 |
| 其他 peer 通信 | 52 |

### results 表 (131 条)

| 角色 | 结果数 |
|------|--------|
| qa | 39 |
| frontend | 36 |
| core | 25 |
| net | 17 |
| reviewer | 9 |
| session | 5 |

---

## 预防措施

1. **已在 0.3.30 修复**：send 假阴性（matched=true=delivered）+ CLAUDE_CONFIG_DIR 隔离
2. **已在 0.3.32 修复**：team-agent claude 在 tmux 内用 ExecProvider
3. **定期备份 runtime**：`cp -r .team/runtime/ .team/runtime-backup-$(date +%Y%m%d)/`
4. **不要因 send.failed 就 remove-agent**：先等回复，用 inbox 确认
5. **codex session 不持久**：重要产出让 worker report_result 保存
