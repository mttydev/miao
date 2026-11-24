//! Injected context is a history fact, not an operator inbox item.
use crate::{
    protocol::Error,
    store::{append, write_tx, Store},
};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};

/// The wrapper the provider reads and the transcript splits on. Context the
/// engine wrote is attributed to its source rather than presented as words the
/// operator typed, and keeping the shape in one function keeps every reader —
/// the provider prompt, the TUI, a promotion — agreeing on it.
pub fn wrap(source: &str, text: &str) -> Result<String, serde_json::Error> {
    Ok(format!(
        "<injected-context source={}>\n{}\n</injected-context>",
        serde_json::to_string(source)?,
        text
    ))
}

/// Inputs the runner admits on its own behalf to keep a task moving. Their
/// promotion is guidance the operator never wrote, so it is stamped as engine
/// context instead of appearing in history as their prompt.
pub fn continuation_input(id: &str) -> bool {
    ["goal:", "empty:", "transport:", "verify:"]
        .iter()
        .any(|prefix| id.starts_with(prefix))
}

impl Store {
    pub async fn inject(
        &self,
        session: &str,
        id: &str,
        text: &str,
        source: &str,
    ) -> Result<Value, Error> {
        if id.is_empty()
            || id.len() > 256
            || text.is_empty()
            || text.len() > 1024 * 1024
            || source.is_empty()
            || source.len() > 256
            || source.chars().any(char::is_control)
        {
            return Err(Error::Invalid("invalid synthetic context".into()));
        }
        let (session, id, text, source) = (
            session.to_owned(),
            id.to_owned(),
            text.to_owned(),
            source.to_owned(),
        );
        self.call(move |connection| {
            let tx=write_tx(connection)?;
            let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM engine_session WHERE id=?1)",[&session],|row|row.get(0))?;
            if !exists { return Err(Error::Invalid("Session does not exist".into())); }
            let existing:Option<(u64,String)>=tx.query_row("SELECT seq,data FROM engine_event WHERE session_id=?1 AND kind='message.synthetic' AND json_extract(data,'$.input_id')=?2 ORDER BY seq LIMIT 1",params![session,id],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
            if let Some((seq,data))=existing {
                let data:Value=serde_json::from_str(&data)?;
                if data["text"]!=text || data["source"]!=source {return Err(Error::Conflict);}
                return Ok(json!({"seq":seq,"duplicate":true}));
            }
            let event=append(&tx,&session,"message.synthetic",json!({"input_id":id,"text":text,"source":source}))?;
            // The provider sees explicitly attributed context; the transcript
            // projects the synthetic event, never an operator prompt.
            let content=json!([{"type":"text","text":wrap(&source,&text)?}]);
            tx.execute("INSERT INTO engine_message(session_id,seq,role,content,recorded_at_ms,kind) VALUES(?1,?2,'user',?3,?4,?5)",params![session,event.seq,serde_json::to_string(&content)?,event.recorded_at_ms,event.kind])?;
            crate::store::refresh_preview(&tx,&session,&content)?;
            tx.commit()?;
            Ok(json!({"seq":event.seq,"duplicate":false}))
        }).await
    }
}
