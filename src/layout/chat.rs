/// AI 对话消息模型

#[derive(Clone, PartialEq)]
pub enum ChatRole {
    User,
    Assistant,
}

#[derive(Clone)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
    /// 正在等待 AI 响应
    pub loading: bool,
}
