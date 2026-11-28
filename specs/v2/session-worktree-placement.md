# 会话工作树 placement

状态：已更新（2026-11-28）。默认不再自动建树；隔离是显式选择。

## 2026-11-28 更新：默认 Local，worktree 显式

0.5.0 的自动 worktree 回退已取消，对齐 Claude Code 的本地会话：Session 默认使用启动
目录（engine root），多个 Session 可以共享同一检出。只有显式 `worktree_enter` 才把
Session 迁到 `.miao-worktrees/<name>`，并检出真实分支 `miao/<name>`；提交因此可达、
可 review、可 merge，不再是 detached。隔离检查仍然强制，但只作用于已进入工作树的
Session。启动时丢失的工作树优先在原分支重建（已有分支不 reset），detached 仅作兜底。
并发共享检出是操作者的选择；需要隔离时用 `worktree_enter`，或先开一个工作树。

## 问题

一个 git 检出一份 HEAD、一份索引、一份工作区。多个 Session 共用时，任何一条会话的
`checkout`、`cherry-pick`、`reset` 都会移动另一条会话脚下的分支；一条会话的未提交改动
也会被另一条会话的命令覆盖。模型看到别人中途的 `detached HEAD` 或冲突后只能停下，而
停下不是正确性：另一条会话已经可能被破坏。

`worktree_create` 只建树，不改变当前 Session 的 root；`MoveSession` 是跨库 export-move
且拒绝 running Session。两者都不能在一条会话运行中安全地把它挪走。

## 不变量

1. 一个 git 检出默认可以被多个 Session 使用；隔离是显式选择。`worktree_enter` 把
   Session 放到 `.miao-worktrees/<name>` 的分支 `miao/<name>`。`engine_checkout_lease`
   不再决定 placement，表保留给旧库和工具兼容。
2. Session 的读写根由 placement 决定。`engine_location.root` 是每 Session 的工作根；
   `Tools` 的 scoped 克隆用该 root 解析路径、bash cwd、后台作业和快照。
3. 隔离是强制的，但只作用于工作树 Session。工作树 Session 不能写主检出，也不能用
   `git -C`、`--git-dir`、`GIT_DIR`、`GIT_WORK_TREE` 或 `cd` 把 git 指回主检出。

## 实现

### 存储

`engine_location(session_id, root)` 已经是每 Session 一行。新增
`engine_checkout_lease(root PRIMARY KEY, session_id, pid, acquired_at_ms)`：

- `acquire_checkout_lease` 在一个写事务里决定主检出归属；已有 owner 且未归档/未删除时
  返回 false。
- 归档、删除、主动 `worktree_enter` 都释放租约。
- 启动时 `seed_checkout_lease` 把老数据库的主检出租约补给最早的非归档 Session，避免新
  Session 抢走老会话正在使用的检出。

### placement

> 以下为 0.5.0 的历史实现，已被 2026-11-28 更新取代：新 Session 不再自动建树，默认
> 留在启动目录；`worktree_enter` 才是显式隔离，且使用真实分支。

`Runtime::initialize` 在 `placement` 锁内：

- 已有 root 的 Session 沿用该 root；如果 root 是引擎检出但租约已被别人拿走，则先建
  工作树并更新 root，再初始化。
- 新 Session 先尝试租约；拿不到就在主检出的 `.miao-worktrees/<session>` 下建
  `git worktree add --detach`，把该路径写进 `engine_location`。
- 非 git 工作区没有共享 HEAD，保持原目录。
- 建树失败且目录不存在时失败关闭，绝不回落到共享检出。

`create_detached` 建 detach 工作树，不占用分支；建树后把 `/.miao-worktrees/` 写进
`.git/info/exclude`，因此不污染任何仓库的 `git status`，也不要求修改用户的 `.gitignore`。

### 工具

`Tools` 增加 `base_root`、每 Session 的 `root/directory/main_checkout` 映射：

- `with_read_scope(session)` 同时切换 root/directory/main_checkout；没有 placement 的
  会话回落到引擎 root。
- 主回合、读批、后台作业都通过 scoped 克隆 prepare/execute，因此路径包含检查天然拒绝
  写到工作树外。
- `deny_checkout_redirect` 只对 process 工具生效，拦截指向主检出的 git；`git diff A..B`
  这种范围不属于路径，不会被误杀。
- `worktree_enter` 让已经在共享检出里的 Session 在运行中迁到自己的工作树；它更新
  `engine_location.root`、注册新的目录句柄并释放原租约。storage 不搬家，因为 Session
  的存储本来就在同一个引擎库里。

### 恢复

- 启动时把 `engine_location` 的全部 root 注册到 `Tools`。
- 记录的工作树被外部删除时，启动时重新建树并更新 root，不静默回落到主检出。
- 工作树 Session 的系统提示追加 `<worktree root=...>`，明确它写的是哪棵树、哪棵检出
  是它分离出来的。

## 验收

- 真 git 仓库、同一引擎两个 Session：第一个 root 是检出，第二个 root 在
  `.miao-worktrees/` 下；主检出 HEAD 与 status 不变。
- 第二个 Session 写主检出文件被拒；`git -C 主检出 ...`、`cd .. && git ...`、
  `GIT_DIR=../.git git ...` 被拒；工作树内 `git status`、`git -C .` 允许。
- `crates/miao-engine/tests/worktree_isolation.rs` 与
  `tools::worktree_tool_tests` 覆盖以上路径。
