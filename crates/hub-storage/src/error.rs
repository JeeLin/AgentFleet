use thiserror::Error;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("r2d2 pool error: {0}")]
    Pool(#[from] r2d2::Error),

    #[error("migration {version} failed: {source}")]
    Migration {
        version: i64,
        #[source]
        source: rusqlite::Error,
    },

    #[error("schema version {found} is newer than supported {supported}: refusing to open an older binary against a newer database")]
    SchemaTooNew { found: i64, supported: i64 },

    #[error("no home or data directory could be resolved")]
    NoDataDir,

    /// 库里的 `device.status` 不是 'online'/'offline'。
    ///
    /// schema 上有 `CHECK` 约束，正常途径写不进来；出现即说明库被外部改动，
    /// 或迁移漏了约束。**不静默回落 offline**——那会掩盖数据损坏。
    #[error("invalid device.status in database: {0:?}")]
    InvalidDeviceStatus(String),

    /// 库里的 `session.state` 不是 'running'/'ended'。
    ///
    /// 与 [`StorageError::InvalidDeviceStatus`] 同一个理由：把它当成 running
    /// 会让一个已结束的会话继续被算作在跑，而页面上没有任何东西能提示用户。
    #[error("invalid session.state in database: {0:?}")]
    InvalidSessionState(String),

    /// 库里的 `session.base_rev_status` 不在五态里。
    ///
    /// **不静默回落 `Unknown`**：那会把「库里存了一个我们不认识的值」（数据
    /// 损坏 / 迁移漏了 / 有人手工改库）显示成「这是 v0.6.0 之前的老会话」，
    /// 一个看起来正常的谎话。与上面两条同一条口径。
    #[error("invalid session.base_rev_status in database: {0:?}")]
    InvalidBaseRevStatus(String),

    /// 工作区路径不合法。`reason` 是**规则**而不是「非法」——
    /// 页面上要显示它，用户得知道该改成什么。
    #[error("invalid workspace path {path:?}: {reason}")]
    InvalidWorkspacePath { path: String, reason: &'static str },

    /// 子路径越出工作区根。同上：带用户给的那串路径，不带设备绝对路径的根
    /// （设计规范 §7.4 要求越界可见又不泄漏布局）。
    #[error("path {path:?} is outside the workspace root: {reason}")]
    PathOutsideRoot { path: String, reason: &'static str },

    /// 删工作区时仍有会话指着它。
    #[error("workspace {0:?} still has sessions attached")]
    WorkspaceInUse(String),

    /// 给工作区指定了一个库里不存在的设备。
    ///
    /// **这一条是 v3 去外键后补上的**：v2 靠 `REFERENCES device(id)` 拒绝，
    /// 报的是 `FOREIGN KEY constraint failed`——用户看不懂，也不知道是哪个
    /// `node_id` 出的问题。现在由仓储层显式查设备表并给出可读错误。
    ///
    /// 判据仍然是「工作区必须挂在一台真实存在的设备上」：否则页面按节点分组
    /// 会多出一个点不进去的分组。
    #[error("no such device {0:?}: a workspace must belong to a device that exists")]
    NoSuchDevice(String),

    /// 结束一个不存在的会话。
    #[error("no such session: {0}")]
    NoSuchSession(String),

    /// 库里的 `project.shape` 不在三态里。
    ///
    /// 与 `InvalidSessionState` 同一条口径：**不静默回落 `central`**。回落会让
    /// 一个隔离式项目在下次读取时声称「代码在 Hub 上」，而 Hub 上什么都没有。
    #[error("invalid project.shape in database: {0:?}")]
    InvalidProjectShape(String),

    /// 库里的 `project.theme_color` 不在五色里。
    ///
    /// 回落成灰色会把「库里存了一个我们不认识的颜色」显示成「用户选了灰色」，
    /// 一个能点得动的谎话。
    #[error("invalid project.theme_color in database: {0:?}")]
    InvalidThemeColor(String),

    /// 库里没有这个项目。
    #[error("no such project: {id:?}")]
    ProjectNotFound { id: String },

    /// 形态变更被拒。**带 `from` / `to` 两端的值**，因为拒绝的理由取决于它们：
    /// `isolated → central` 是允许的，`central → isolated` 才是「产品做不到
    /// 把已经上传的代码收回」。只报「不允许」用户不知道该改成什么。
    #[error("refused to change project shape from {from:?} to {to:?}")]
    ShapeTransitionRefused {
        from: &'static str,
        to: &'static str,
    },

    /// 库里的 `binding.cleanup_policy` 不在三档里。与其余取值校验同一条口径。
    #[error("invalid binding.cleanup_policy in database: {0:?}")]
    InvalidCleanupPolicy(String),

    /// 库里的 `task.state` 不在六态里。
    #[error("invalid task.state in database: {0:?}")]
    InvalidTaskState(String),

    /// 库里的 `execution.state` 不在五态里。
    #[error("invalid execution.state in database: {0:?}")]
    InvalidExecutionState(String),

    /// 库里没有这个任务。
    #[error("no such task: {id:?}")]
    TaskNotFound { id: String },

    /// 库里没有这次执行。
    #[error("no such execution: {id:?}")]
    ExecutionNotFound { id: String },

    /// 会话已经挂到别的一次执行下。
    ///
    /// 归属只写一次：中途改会让执行的历史归属对不上。
    #[error("session {session:?} is already attached to execution {existing:?}")]
    SessionAlreadyAttached { session: String, existing: String },

    /// 没有这条绑定。
    #[error("no such binding: project {project_id:?} on device {device_id:?}")]
    NoSuchBinding {
        project_id: String,
        device_id: String,
    },

    /// 改固定目录被拒：该绑定下已有执行记录，它们的 worktree 指向旧目录。
    ///
    /// 带 `executions` 条数——用户得知道是"12 条执行记录挡着"还是"1 条"，
    /// 只有数字能让他判断该不该先清理。
    #[error(
        "refused to change the fixed directory of project {project_id:?} on device \
         {device_id:?}: {executions} execution record(s) still live in the old directory"
    )]
    FixedDirInUse {
        project_id: String,
        device_id: String,
        executions: i64,
    },

    /// 起点已经写过一个不同的值。
    ///
    /// 起点是这次执行的历史事实，`set_base_commit` 只填 `NULL`，不改已有的值。
    /// 撞上这一条说明有两条路径都试图定起点——那是要修的问题，不是能盖过去的事。
    #[error("execution {id:?} already has base commit {existing:?}: refusing to overwrite it")]
    BaseCommitAlreadySet { id: String, existing: String },

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, StorageError>;
