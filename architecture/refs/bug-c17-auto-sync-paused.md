# C17: auto_sync_paused 暂停不生效

**发现来源:** architecture/review-final.md C17
**维度引用:** §6 操作矩阵 :361, §9 脏状态 DIRTY-16
**严重度:** P0/high — 功能 bug

**现象:** UI 的"暂停自动同步"开关实际不会停止 scanner/watcher 触发的自动同步。
**状态:** 待修复，需派 core。
