use serde::Serialize;

/// 统一错误类型：网页 API 只返回 `{ code, message }`。
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    Msg(String),
    #[error("网络请求失败: {0}")]
    Http(#[from] reqwest::Error),
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON 解析失败: {0}")]
    Json(#[from] serde_json::Error),
}

impl AppError {
    pub fn msg(s: impl Into<String>) -> Self {
        AppError::Msg(s.into())
    }

    fn code(&self) -> &'static str {
        match self {
            AppError::Msg(_) => "APP",
            AppError::Http(_) => "HTTP",
            AppError::Io(_) => "IO",
            AppError::Json(_) => "JSON",
        }
    }
}

impl Serialize for AppError {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("AppError", 2)?;
        st.serialize_field("code", self.code())?;
        st.serialize_field("message", &self.to_string())?;
        st.end()
    }
}

pub type AppResult<T> = Result<T, AppError>;
