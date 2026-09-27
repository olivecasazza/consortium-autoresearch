//! Inter-node messaging protocol.
//!
//! Rust implementation of `ClusterShell.Communication`.
//!
//! Messages are the fundamental units of communication between nodes in the
//! propagation tree. They are serialized to/from XML for transmission over
//! SSH channels. Each message has a type identifier, a unique message ID,
//! optional attributes, and an optional payload (base64-encoded bytes).
//!
//! The [`Channel`] struct manages bidirectional communication using an
//! [`XmlReader`] for parsing incoming XML and XML generation for outgoing
//! messages.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;

// ============================================================================
// Constants
// ============================================================================

/// XML character encoding
pub const ENCODING: &str = "utf-8";

/// Default base64 line length for large payloads
pub const DEFAULT_B64_LINE_LENGTH: usize = 65536;

// Channel stream name constants
/// Stream name for the writer side of a channel
pub const SNAME_WRITER: &str = "ch-writer";
/// Stream name for the reader side of a channel
pub const SNAME_READER: &str = "ch-reader";
/// Stream name for the error side of a channel
pub const SNAME_ERROR: &str = "ch-error";

// Message type identifier constants
const IDENT_GEN: &str = "GEN";
const IDENT_CFG: &str = "CFG";
const IDENT_CTL: &str = "CTL";
const IDENT_ACK: &str = "ACK";
const IDENT_ERR: &str = "ERR";
const IDENT_OUT: &str = "OUT";
const IDENT_SER: &str = "SER";
const IDENT_RET: &str = "RET";
const IDENT_TIM: &str = "TIM";
const IDENT_RTR: &str = "RTR";
const IDENT_CHA: &str = "CHA";
const IDENT_END: &str = "END";

// ============================================================================
// Errors
// ============================================================================

/// Errors raised during message processing.
#[derive(Error, Debug)]
pub enum MessageProcessingError {
    /// An unknown or invalid message type was encountered.
    #[error("Unknown message type: {0}")]
    UnknownType(String),

    /// A required attribute is missing from a message.
    #[error("Invalid message attributes: missing key \"{0}\"")]
    MissingAttribute(String),

    /// An unexpected payload was found on a message that doesn't support one.
    #[error("Got unexpected payload for Message {0}")]
    UnexpectedPayload(String),

    /// The message payload is invalid or corrupted.
    #[error("Message {0} has an invalid payload")]
    InvalidPayload(String),

    /// XML parse error.
    #[error("Parse error: {0}")]
    ParseError(String),

    /// Invalid starting XML tag.
    #[error("Invalid starting tag {0}")]
    InvalidTag(String),
}

// ============================================================================
// Global message ID counter
// ============================================================================

static MSG_COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_msg_id() -> u64 {
    MSG_COUNTER.fetch_add(1, Ordering::Relaxed)
}

// ============================================================================
// Minimal base64
// ============================================================================

/// Simple base64 alphabet (standard RFC 4648).
const B64_CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode bytes to base64 string.
pub fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;

        out.push(B64_CHARS[((triple >> 18) & 0x3F) as usize] as char);
        out.push(B64_CHARS[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64_CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(B64_CHARS[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Decode base64 string to bytes. Ignores whitespace/newlines (RFC 4648 relaxed).
pub fn base64_decode(input: &str) -> Result<Vec<u8>, MessageProcessingError> {
    fn b64_val(c: u8) -> Result<u8, MessageProcessingError> {
        match c {
            b'A'..=b'Z' => Ok(c - b'A'),
            b'a'..=b'z' => Ok(c - b'a' + 26),
            b'0'..=b'9' => Ok(c - b'0' + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err(MessageProcessingError::InvalidPayload(
                "invalid base64 character".into(),
            )),
        }
    }

    // Strip whitespace
    let clean: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if clean.is_empty() {
        return Ok(Vec::new());
    }

    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    for chunk in clean.chunks(4) {
        if chunk.len() < 2 {
            return Err(MessageProcessingError::InvalidPayload(
                "truncated base64".into(),
            ));
        }
        let a = b64_val(chunk[0])?;
        let b = b64_val(chunk[1])?;
        out.push((a << 2) | (b >> 4));
        if chunk.len() > 2 && chunk[2] != b'=' {
            let c = b64_val(chunk[2])?;
            out.push((b << 4) | (c >> 2));
            if chunk.len() > 3 && chunk[3] != b'=' {
                let d = b64_val(chunk[3])?;
                out.push((c << 6) | d);
            }
        }
    }
    Ok(out)
}

// ============================================================================
// XML escape helpers
// ============================================================================

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

fn xml_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

// ============================================================================
// Message
// ============================================================================

/// A communication message in the propagation tree protocol.
///
/// Messages are tagged unions representing the different message types
/// exchanged between ClusterShell nodes. Each variant carries the fields
/// specific to that message type.
#[derive(Debug, Clone)]
pub enum Message {
    /// Generic base message.
    General { msgid: u64, data: Option<Vec<u8>> },

    /// Configuration propagation container (ident = "CFG").
    /// Has payload. Carries serialized topology.
    Configuration {
        msgid: u64,
        gateway: String,
        data: Option<Vec<u8>>,
    },

    /// Action request routed through the tree (ident = "CTL").
    /// Has payload. Carries serialized command data.
    Control {
        msgid: u64,
        srcid: u64,
        action: String,
        target: String,
        data: Option<Vec<u8>>,
    },

    /// Acknowledgement message (ident = "ACK").
    Ack { msgid: u64, ack: u64 },

    /// Error message (ident = "ERR").
    Error { msgid: u64, reason: String },

    /// Standard output container (ident = "OUT").
    /// Has payload. Carries encoded output data.
    StdOut {
        msgid: u64,
        srcid: u64,
        nodes: String,
        data: Option<Vec<u8>>,
    },

    /// Standard error container (ident = "SER").
    /// Has payload. Carries encoded stderr data.
    StdErr {
        msgid: u64,
        srcid: u64,
        nodes: String,
        data: Option<Vec<u8>>,
    },

    /// Return code container (ident = "RET").
    Retcode {
        msgid: u64,
        srcid: u64,
        retcode: i32,
        nodes: String,
    },

    /// Timeout notification (ident = "TIM").
    Timeout {
        msgid: u64,
        srcid: u64,
        nodes: String,
    },

    /// Routing notification (ident = "RTR").
    Routing {
        msgid: u64,
        srcid: u64,
        event: String,
        gateway: String,
        targets: String,
    },

    /// Start of channel communication (ident = "CHA").
    Start { msgid: u64 },

    /// End of channel communication (ident = "END").
    End { msgid: u64 },
}

impl Message {
    // -- Constructors --------------------------------------------------------

    /// Create a new General message.
    pub fn general() -> Self {
        Message::General {
            msgid: next_msg_id(),
            data: None,
        }
    }

    /// Create a new Configuration message.
    pub fn configuration(gateway: &str) -> Self {
        Message::Configuration {
            msgid: next_msg_id(),
            gateway: gateway.to_string(),
            data: None,
        }
    }

    /// Create a new Control message.
    pub fn control(srcid: u64) -> Self {
        Message::Control {
            msgid: next_msg_id(),
            srcid,
            action: String::new(),
            target: String::new(),
            data: None,
        }
    }

    /// Create a new Ack message.
    pub fn ack(ackid: u64) -> Self {
        Message::Ack {
            msgid: next_msg_id(),
            ack: ackid,
        }
    }

    /// Create a new Error message.
    pub fn error(reason: &str) -> Self {
        Message::Error {
            msgid: next_msg_id(),
            reason: reason.to_string(),
        }
    }

    /// Create a new StdOut message.
    pub fn stdout(nodes: &str, srcid: u64) -> Self {
        Message::StdOut {
            msgid: next_msg_id(),
            srcid,
            nodes: nodes.to_string(),
            data: None,
        }
    }

    /// Create a new StdErr message.
    pub fn stderr(nodes: &str, srcid: u64) -> Self {
        Message::StdErr {
            msgid: next_msg_id(),
            srcid,
            nodes: nodes.to_string(),
            data: None,
        }
    }

    /// Create a new Retcode message.
    pub fn retcode(nodes: &str, retcode: i32, srcid: u64) -> Self {
        Message::Retcode {
            msgid: next_msg_id(),
            srcid,
            retcode,
            nodes: nodes.to_string(),
        }
    }

    /// Create a new Timeout message.
    pub fn timeout(nodes: &str, srcid: u64) -> Self {
        Message::Timeout {
            msgid: next_msg_id(),
            srcid,
            nodes: nodes.to_string(),
        }
    }

    /// Create a new Routing message.
    pub fn routing(event: &str, gateway: &str, targets: &str, srcid: u64) -> Self {
        Message::Routing {
            msgid: next_msg_id(),
            srcid,
            event: event.to_string(),
            gateway: gateway.to_string(),
            targets: targets.to_string(),
        }
    }

    /// Create a Start message.
    pub fn start() -> Self {
        Message::Start {
            msgid: next_msg_id(),
        }
    }

    /// Create an End message.
    pub fn end() -> Self {
        Message::End {
            msgid: next_msg_id(),
        }
    }

    // -- Accessors -----------------------------------------------------------

    /// Get the message type identifier string.
    pub fn ident(&self) -> &str {
        match self {
            Message::General { .. } => IDENT_GEN,
            Message::Configuration { .. } => IDENT_CFG,
            Message::Control { .. } => IDENT_CTL,
            Message::Ack { .. } => IDENT_ACK,
            Message::Error { .. } => IDENT_ERR,
            Message::StdOut { .. } => IDENT_OUT,
            Message::StdErr { .. } => IDENT_SER,
            Message::Retcode { .. } => IDENT_RET,
            Message::Timeout { .. } => IDENT_TIM,
            Message::Routing { .. } => IDENT_RTR,
            Message::Start { .. } => IDENT_CHA,
            Message::End { .. } => IDENT_END,
        }
    }

    /// Get the message ID.
    pub fn msgid(&self) -> u64 {
        match self {
            Message::General { msgid, .. }
            | Message::Configuration { msgid, .. }
            | Message::Control { msgid, .. }
            | Message::Ack { msgid, .. }
            | Message::Error { msgid, .. }
            | Message::StdOut { msgid, .. }
            | Message::StdErr { msgid, .. }
            | Message::Retcode { msgid, .. }
            | Message::Timeout { msgid, .. }
            | Message::Routing { msgid, .. }
            | Message::Start { msgid, .. }
            | Message::End { msgid, .. } => *msgid,
        }
    }

    /// Whether this message type carries a payload.
    pub fn has_payload(&self) -> bool {
        matches!(
            self,
            Message::General { .. }
                | Message::Configuration { .. }
                | Message::Control { .. }
                | Message::StdOut { .. }
                | Message::StdErr { .. }
        )
    }

    /// Get a reference to the payload data, if any.
    pub fn data(&self) -> Option<&[u8]> {
        match self {
            Message::General { data, .. }
            | Message::Configuration { data, .. }
            | Message::Control { data, .. }
            | Message::StdOut { data, .. }
            | Message::StdErr { data, .. } => data.as_deref(),
            _ => None,
        }
    }

    /// Set the payload data (raw bytes, will be base64-encoded on serialization).
    pub fn set_data(&mut self, payload: Vec<u8>) {
        match self {
            Message::General { data, .. }
            | Message::Configuration { data, .. }
            | Message::Control { data, .. }
            | Message::StdOut { data, .. }
            | Message::StdErr { data, .. } => {
                *data = Some(payload);
            }
            _ => {}
        }
    }

    /// Encode arbitrary bytes as base64 and store as payload.
    pub fn data_encode(&mut self, raw: &[u8]) {
        let encoded = base64_encode(raw);
        self.set_data(encoded.into_bytes());
    }

    /// Decode the base64 payload back to raw bytes.
    pub fn data_decode(&self) -> Result<Vec<u8>, MessageProcessingError> {
        match self.data() {
            Some(d) => {
                let s = std::str::from_utf8(d)
                    .map_err(|_| MessageProcessingError::InvalidPayload(self.ident().into()))?;
                base64_decode(s)
            }
            None => Err(MessageProcessingError::InvalidPayload(self.ident().into())),
        }
    }

    /// Append data during incremental XML parsing.
    pub fn data_update(&mut self, raw: &[u8]) -> Result<(), MessageProcessingError> {
        if !self.has_payload() {
            return Err(MessageProcessingError::UnexpectedPayload(
                self.ident().into(),
            ));
        }
        match self {
            Message::General { data, .. }
            | Message::Configuration { data, .. }
            | Message::Control { data, .. }
            | Message::StdOut { data, .. }
            | Message::StdErr { data, .. } => {
                if let Some(ref mut d) = data {
                    d.extend_from_slice(raw);
                } else {
                    *data = Some(raw.to_vec());
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Get the srcid for routed messages, None for non-routed.
    pub fn srcid(&self) -> Option<u64> {
        match self {
            Message::Control { srcid, .. }
            | Message::StdOut { srcid, .. }
            | Message::StdErr { srcid, .. }
            | Message::Retcode { srcid, .. }
            | Message::Timeout { srcid, .. }
            | Message::Routing { srcid, .. } => Some(*srcid),
            _ => None,
        }
    }

    /// Get the nodes field if present.
    pub fn nodes(&self) -> Option<&str> {
        match self {
            Message::StdOut { nodes, .. }
            | Message::StdErr { nodes, .. }
            | Message::Retcode { nodes, .. }
            | Message::Timeout { nodes, .. } => Some(nodes.as_str()),
            _ => None,
        }
    }

    // -- XML serialization ---------------------------------------------------

    /// Serialize this message to XML bytes.
    ///
    /// Produces output like:
    /// `<message type="CFG" msgid="0" gateway="gw1">base64payload</message>`
    pub fn xml(&self) -> Vec<u8> {
        let mut attrs = Vec::new();

        // type and msgid are always present
        attrs.push(format!("type=\"{}\"", xml_escape(self.ident())));
        attrs.push(format!("msgid=\"{}\"", self.msgid()));

        // variant-specific attributes
        match self {
            Message::Configuration { gateway, .. } => {
                attrs.push(format!("gateway=\"{}\"", xml_escape(gateway)));
            }
            Message::Control {
                srcid,
                action,
                target,
                ..
            } => {
                attrs.push(format!("srcid=\"{}\"", srcid));
                attrs.push(format!("action=\"{}\"", xml_escape(action)));
                attrs.push(format!("target=\"{}\"", xml_escape(target)));
            }
            Message::Ack { ack, .. } => {
                attrs.push(format!("ack=\"{}\"", ack));
            }
            Message::Error { reason, .. } => {
                attrs.push(format!("reason=\"{}\"", xml_escape(reason)));
            }
            Message::StdOut { srcid, nodes, .. } => {
                attrs.push(format!("srcid=\"{}\"", srcid));
                attrs.push(format!("nodes=\"{}\"", xml_escape(nodes)));
            }
            Message::StdErr { srcid, nodes, .. } => {
                attrs.push(format!("srcid=\"{}\"", srcid));
                attrs.push(format!("nodes=\"{}\"", xml_escape(nodes)));
            }
            Message::Retcode {
                srcid,
                retcode,
                nodes,
                ..
            } => {
                attrs.push(format!("srcid=\"{}\"", srcid));
                attrs.push(format!("retcode=\"{}\"", retcode));
                attrs.push(format!("nodes=\"{}\"", xml_escape(nodes)));
            }
            Message::Timeout { srcid, nodes, .. } => {
                attrs.push(format!("srcid=\"{}\"", srcid));
                attrs.push(format!("nodes=\"{}\"", xml_escape(nodes)));
            }
            Message::Routing {
                srcid,
                event,
                gateway,
                targets,
                ..
            } => {
                attrs.push(format!("srcid=\"{}\"", srcid));
                attrs.push(format!("event=\"{}\"", xml_escape(event)));
                attrs.push(format!("gateway=\"{}\"", xml_escape(gateway)));
                attrs.push(format!("targets=\"{}\"", xml_escape(targets)));
            }
            _ => {}
        }

        let attr_str = attrs.join(" ");
        let payload = self.data().unwrap_or(&[]);

        if payload.is_empty() {
            format!("<message {}/>", attr_str).into_bytes()
        } else {
            let payload_str = String::from_utf8_lossy(payload);
            format!("<message {}>{}</message>", attr_str, payload_str).into_bytes()
        }
    }

    /// Build a message from parsed XML attributes.
    ///
    /// The message type is selected based on the "type" attribute, and all
    /// other attributes are read according to the variant's schema.
    pub fn from_attrs(attrs: &HashMap<String, String>) -> Result<Message, MessageProcessingError> {
        let msg_type = attrs
            .get("type")
            .ok_or_else(|| MessageProcessingError::MissingAttribute("type".into()))?;

        let msgid: u64 = attrs
            .get("msgid")
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(next_msg_id);

        match msg_type.as_str() {
            IDENT_CFG => {
                let gateway = attrs.get("gateway").cloned().unwrap_or_default();
                Ok(Message::Configuration {
                    msgid,
                    gateway,
                    data: None,
                })
            }
            IDENT_CTL => {
                let srcid = attr_u64(attrs, "srcid")?;
                let action = attrs.get("action").cloned().unwrap_or_default();
                let target = attrs.get("target").cloned().unwrap_or_default();
                Ok(Message::Control {
                    msgid,
                    srcid,
                    action,
                    target,
                    data: None,
                })
            }
            IDENT_ACK => {
                let ack = attr_u64(attrs, "ack")?;
                Ok(Message::Ack { msgid, ack })
            }
            IDENT_ERR => {
                let reason = attrs.get("reason").cloned().unwrap_or_default();
                Ok(Message::Error { msgid, reason })
            }
            IDENT_OUT => {
                let srcid = attr_u64(attrs, "srcid")?;
                let nodes = attrs.get("nodes").cloned().unwrap_or_default();
                Ok(Message::StdOut {
                    msgid,
                    srcid,
                    nodes,
                    data: None,
                })
            }
            IDENT_SER => {
                let srcid = attr_u64(attrs, "srcid")?;
                let nodes = attrs.get("nodes").cloned().unwrap_or_default();
                Ok(Message::StdErr {
                    msgid,
                    srcid,
                    nodes,
                    data: None,
                })
            }
            IDENT_RET => {
                let srcid = attr_u64(attrs, "srcid")?;
                let retcode: i32 = attrs
                    .get("retcode")
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| MessageProcessingError::MissingAttribute("retcode".into()))?;
                let nodes = attrs.get("nodes").cloned().unwrap_or_default();
                Ok(Message::Retcode {
                    msgid,
                    srcid,
                    retcode,
                    nodes,
                })
            }
            IDENT_TIM => {
                let srcid = attr_u64(attrs, "srcid")?;
                let nodes = attrs.get("nodes").cloned().unwrap_or_default();
                Ok(Message::Timeout {
                    msgid,
                    srcid,
                    nodes,
                })
            }
            IDENT_RTR => {
                let srcid = attr_u64(attrs, "srcid")?;
                let event = attrs.get("event").cloned().unwrap_or_default();
                let gateway = attrs.get("gateway").cloned().unwrap_or_default();
                let targets = attrs.get("targets").cloned().unwrap_or_default();
                Ok(Message::Routing {
                    msgid,
                    srcid,
                    event,
                    gateway,
                    targets,
                })
            }
            other => Err(MessageProcessingError::UnknownType(other.into())),
        }
    }
}

impl std::fmt::Display for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Message {} (type: {}, msgid: {})",
            self.ident(),
            self.ident(),
            self.msgid()
        )
    }
}

/// Helper: parse a u64 attribute.
fn attr_u64(attrs: &HashMap<String, String>, key: &str) -> Result<u64, MessageProcessingError> {
    attrs
        .get(key)
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| MessageProcessingError::MissingAttribute(key.into()))
}

// ============================================================================
// XmlReader — incremental SAX-style XML parser
// ============================================================================

/// Parser states for the simple XML state machine.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ParserState {
    /// Outside any tag, looking for '<'.
    Text,
    /// Inside a tag name (after '<').
    TagName,
    /// Inside a closing tag (after '</').
    CloseTagName,
    /// Reading attributes inside an opening tag.
    Attributes,
    /// Inside a self-closing tag (seen '/').
    SelfClose,
}

/// Incremental XML reader that parses fragments and produces [`Message`] instances.
///
/// Modeled after Python's SAX `ContentHandler`. Feed XML data incrementally
/// via [`feed()`](XmlReader::feed) and retrieve parsed messages from the queue.
#[derive(Debug)]
pub struct XmlReader {
    msg_queue: VecDeque<Message>,
    /// Protocol version from `<channel version="...">`.
    pub version: Option<String>,
    /// Current message being assembled.
    draft: Option<Message>,
    // Parser internals
    state: ParserState,
    tag_buf: String,
    attr_name: String,
    attr_val: String,
    attrs: HashMap<String, String>,
    text_buf: String,
    in_attr_value: bool,
    attr_quote: char,
}

impl XmlReader {
    /// Create a new empty XmlReader.
    pub fn new() -> Self {
        Self {
            msg_queue: VecDeque::new(),
            version: None,
            draft: None,
            state: ParserState::Text,
            tag_buf: String::new(),
            attr_name: String::new(),
            attr_val: String::new(),
            attrs: HashMap::new(),
            text_buf: String::new(),
            in_attr_value: false,
            attr_quote: '"',
        }
    }

    /// Whether a complete message is available.
    pub fn msg_available(&self) -> bool {
        !self.msg_queue.is_empty()
    }

    /// Pop and return the oldest message from the queue.
    pub fn pop_msg(&mut self) -> Option<Message> {
        self.msg_queue.pop_front()
    }

    /// Feed XML data to the parser.
    ///
    /// This is the incremental entry point. Each call may produce zero or more
    /// messages that can be retrieved with [`pop_msg()`](XmlReader::pop_msg).
    pub fn feed(&mut self, data: &str) -> Result<(), MessageProcessingError> {
        for ch in data.chars() {
            match self.state {
                ParserState::Text => {
                    if ch == '<' {
                        // Flush any accumulated text as character data
                        if !self.text_buf.is_empty() {
                            let text = std::mem::take(&mut self.text_buf);
                            self.characters(&text)?;
                        }
                        self.tag_buf.clear();
                        self.state = ParserState::TagName;
                    } else {
                        self.text_buf.push(ch);
                    }
                }
                ParserState::TagName => {
                    if ch == '/' && self.tag_buf.is_empty() {
                        // Closing tag: </...>
                        self.tag_buf.clear();
                        self.state = ParserState::CloseTagName;
                    } else if ch == ' ' || ch == '\t' {
                        // End of tag name, start reading attributes
                        self.attrs.clear();
                        self.attr_name.clear();
                        self.state = ParserState::Attributes;
                    } else if ch == '>' {
                        // End of opening tag with no attributes
                        self.attrs.clear();
                        self.start_element(&self.tag_buf.clone(), &self.attrs.clone())?;
                        self.state = ParserState::Text;
                    } else if ch == '/' {
                        self.attrs.clear();
                        self.state = ParserState::SelfClose;
                    } else {
                        self.tag_buf.push(ch);
                    }
                }
                ParserState::CloseTagName => {
                    if ch == '>' {
                        self.end_element(&self.tag_buf.clone())?;
                        self.state = ParserState::Text;
                    } else {
                        self.tag_buf.push(ch);
                    }
                }
                ParserState::Attributes => {
                    if self.in_attr_value {
                        if ch == self.attr_quote {
                            // End of attribute value
                            let name = std::mem::take(&mut self.attr_name);
                            let val = xml_unescape(&std::mem::take(&mut self.attr_val));
                            self.attrs.insert(name, val);
                            self.in_attr_value = false;
                        } else {
                            self.attr_val.push(ch);
                        }
                    } else if ch == '=' {
                        // About to read attribute value
                    } else if ch == '"' || ch == '\'' {
                        self.attr_quote = ch;
                        self.attr_val.clear();
                        self.in_attr_value = true;
                    } else if ch == '>' {
                        // End of opening tag
                        self.start_element(&self.tag_buf.clone(), &self.attrs.clone())?;
                        self.state = ParserState::Text;
                    } else if ch == '/' {
                        self.state = ParserState::SelfClose;
                    } else if ch == ' ' || ch == '\t' || ch == '\n' || ch == '\r' {
                        // whitespace between attributes
                    } else {
                        self.attr_name.push(ch);
                    }
                }
                ParserState::SelfClose => {
                    if ch == '>' {
                        // Self-closing tag: treat as open + close
                        let tag = self.tag_buf.clone();
                        let attrs = self.attrs.clone();
                        self.start_element(&tag, &attrs)?;
                        self.end_element(&tag)?;
                        self.state = ParserState::Text;
                    }
                    // else: unexpected char, skip
                }
            }
        }
        Ok(())
    }

    fn start_element(
        &mut self,
        name: &str,
        attrs: &HashMap<String, String>,
    ) -> Result<(), MessageProcessingError> {
        match name {
            "channel" => {
                self.version = attrs.get("version").cloned();
                self.msg_queue.push_back(Message::start());
            }
            "message" => {
                let msg = Message::from_attrs(attrs)?;
                self.draft = Some(msg);
            }
            _ => {
                return Err(MessageProcessingError::InvalidTag(name.into()));
            }
        }
        Ok(())
    }

    fn end_element(&mut self, name: &str) -> Result<(), MessageProcessingError> {
        match name {
            "message" => {
                if let Some(msg) = self.draft.take() {
                    self.msg_queue.push_back(msg);
                }
            }
            "channel" => {
                self.msg_queue.push_back(Message::end());
            }
            _ => {}
        }
        Ok(())
    }

    fn characters(&mut self, content: &str) -> Result<(), MessageProcessingError> {
        if let Some(ref mut draft) = self.draft {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                draft.data_update(trimmed.as_bytes())?;
            }
        }
        Ok(())
    }
}

impl Default for XmlReader {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Channel
// ============================================================================

/// Trait for channel event handling (start/recv).
///
/// Implementors define the logic for initializing a channel and processing
/// incoming messages.
pub trait ChannelHandler: Send {
    /// Called when the channel is connected and ready.
    fn start(&mut self, channel: &mut Channel);

    /// Called when a complete message has been received.
    fn recv(&mut self, channel: &mut Channel, msg: Message);
}

/// Communication channel between nodes in the propagation tree.
///
/// A Channel manages XML-based message exchange over a transport (typically
/// an SSH pipe). It uses an [`XmlReader`] for incoming messages and XML
/// generation for outgoing messages.
#[derive(Debug)]
pub struct Channel {
    /// Whether the channel has been opened (received StartMessage).
    pub opened: bool,
    /// Whether the channel setup is complete (received ACK to config).
    pub setup: bool,
    /// Whether this end initiated the channel.
    pub initiator: bool,
    /// The XML reader for parsing incoming data.
    pub xml_reader: XmlReader,
    /// Outgoing message buffer.
    pub outbox: VecDeque<Vec<u8>>,
}

impl Channel {
    /// Create a new Channel.
    ///
    /// # Arguments
    /// * `initiator` - true if this end is initiating the channel
    pub fn new(initiator: bool) -> Self {
        Self {
            opened: false,
            setup: false,
            initiator,
            xml_reader: XmlReader::new(),
            outbox: VecDeque::new(),
        }
    }

    /// Generate the XML document header.
    pub fn init_xml(&self) -> Vec<u8> {
        b"<?xml version='1.0' encoding='UTF-8'?>".to_vec()
    }

    /// Generate the opening `<channel>` tag.
    pub fn open_xml(&self, version: &str) -> Vec<u8> {
        format!("<channel version=\"{}\">", xml_escape(version)).into_bytes()
    }

    /// Generate the closing `</channel>` tag.
    pub fn close_xml(&self) -> Vec<u8> {
        b"</channel>".to_vec()
    }

    /// Serialize a message and enqueue it for sending.
    pub fn send(&mut self, msg: &Message) {
        let mut xml = msg.xml();
        xml.push(b'\n');
        self.outbox.push_back(xml);
    }

    /// Feed incoming data to the XML reader and return any parsed messages.
    pub fn feed(&mut self, data: &str) -> Result<Vec<Message>, MessageProcessingError> {
        self.xml_reader.feed(data)?;
        let mut messages = Vec::new();
        while let Some(msg) = self.xml_reader.pop_msg() {
            messages.push(msg);
        }
        Ok(messages)
    }

    /// Take the next outgoing message from the outbox.
    pub fn take_outgoing(&mut self) -> Option<Vec<u8>> {
        self.outbox.pop_front()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- Base64 tests --------------------------------------------------------

    #[test]
    fn test_base64_roundtrip() {
        let data = b"Hello, ClusterShell!";
        let encoded = base64_encode(data);
        let decoded = base64_decode(&encoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn test_base64_empty() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_decode("").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn test_base64_padding() {
        // 1 byte -> 4 chars with ==
        let e1 = base64_encode(b"A");
        assert!(e1.ends_with("=="));
        assert_eq!(base64_decode(&e1).unwrap(), b"A");

        // 2 bytes -> 4 chars with =
        let e2 = base64_encode(b"AB");
        assert!(e2.ends_with('=') && !e2.ends_with("=="));
        assert_eq!(base64_decode(&e2).unwrap(), b"AB");

        // 3 bytes -> 4 chars, no padding
        let e3 = base64_encode(b"ABC");
        assert!(!e3.ends_with('='));
        assert_eq!(base64_decode(&e3).unwrap(), b"ABC");
    }

    #[test]
    fn test_base64_with_newlines() {
        // base64 decoder should ignore whitespace
        let encoded = "SGVs\nbG8=";
        let decoded = base64_decode(encoded).unwrap();
        assert_eq!(decoded, b"Hello");
    }

    // -- XML escape tests ----------------------------------------------------

    #[test]
    fn test_xml_escape() {
        assert_eq!(xml_escape("hello"), "hello");
        assert_eq!(xml_escape("a&b"), "a&amp;b");
        assert_eq!(xml_escape("<tag>"), "&lt;tag&gt;");
        assert_eq!(xml_escape("x=\"y\""), "x=&quot;y&quot;");
    }

    #[test]
    fn test_xml_unescape() {
        assert_eq!(xml_unescape("a&amp;b"), "a&b");
        assert_eq!(xml_unescape("&lt;tag&gt;"), "<tag>");
    }

    // -- Message construction tests ------------------------------------------

    #[test]
    fn test_message_idents() {
        assert_eq!(Message::general().ident(), "GEN");
        assert_eq!(Message::configuration("gw1").ident(), "CFG");
        assert_eq!(Message::control(0).ident(), "CTL");
        assert_eq!(Message::ack(0).ident(), "ACK");
        assert_eq!(Message::error("oops").ident(), "ERR");
        assert_eq!(Message::stdout("node1", 0).ident(), "OUT");
        assert_eq!(Message::stderr("node1", 0).ident(), "SER");
        assert_eq!(Message::retcode("node1", 0, 0).ident(), "RET");
        assert_eq!(Message::timeout("node1", 0).ident(), "TIM");
        assert_eq!(Message::routing("ev", "gw", "tgt", 0).ident(), "RTR");
        assert_eq!(Message::start().ident(), "CHA");
        assert_eq!(Message::end().ident(), "END");
    }

    #[test]
    fn test_message_has_payload() {
        assert!(Message::general().has_payload());
        assert!(Message::configuration("gw1").has_payload());
        assert!(Message::control(0).has_payload());
        assert!(Message::stdout("n1", 0).has_payload());
        assert!(Message::stderr("n1", 0).has_payload());
        assert!(!Message::ack(0).has_payload());
        assert!(!Message::error("x").has_payload());
        assert!(!Message::retcode("n1", 0, 0).has_payload());
        assert!(!Message::timeout("n1", 0).has_payload());
        assert!(!Message::start().has_payload());
        assert!(!Message::end().has_payload());
    }

    #[test]
    fn test_message_srcid() {
        assert_eq!(Message::control(42).srcid(), Some(42));
        assert_eq!(Message::stdout("n", 7).srcid(), Some(7));
        assert_eq!(Message::stderr("n", 8).srcid(), Some(8));
        assert_eq!(Message::retcode("n", 0, 9).srcid(), Some(9));
        assert_eq!(Message::timeout("n", 10).srcid(), Some(10));
        assert_eq!(Message::routing("e", "g", "t", 11).srcid(), Some(11));
        assert_eq!(Message::general().srcid(), None);
        assert_eq!(Message::ack(0).srcid(), None);
        assert_eq!(Message::configuration("gw").srcid(), None);
    }

    #[test]
    fn test_message_nodes() {
        assert_eq!(Message::stdout("node[1-3]", 0).nodes(), Some("node[1-3]"));
        assert_eq!(Message::stderr("node5", 0).nodes(), Some("node5"));
        assert_eq!(Message::retcode("node1", 0, 0).nodes(), Some("node1"));
        assert_eq!(Message::timeout("node2", 0).nodes(), Some("node2"));
        assert_eq!(Message::general().nodes(), None);
        assert_eq!(Message::ack(0).nodes(), None);
    }

    // -- Data encode/decode tests -------------------------------------------

    #[test]
    fn test_data_encode_decode() {
        let mut msg = Message::configuration("gw1");
        let payload = b"some topology data here";
        msg.data_encode(payload);
        assert!(msg.data().is_some());
        let decoded = msg.data_decode().unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn test_data_update() {
        let mut msg = Message::configuration("gw");
        msg.data_update(b"SGVs").unwrap();
        msg.data_update(b"bG8=").unwrap();
        assert_eq!(msg.data().unwrap(), b"SGVsbG8=");
        let decoded = msg.data_decode().unwrap();
        assert_eq!(decoded, b"Hello");
    }

    #[test]
    fn test_data_update_unexpected() {
        let mut msg = Message::ack(0);
        let result = msg.data_update(b"data");
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            MessageProcessingError::UnexpectedPayload(_)
        ));
    }

    // -- XML serialization tests --------------------------------------------

    #[test]
    fn test_xml_ack() {
        let msg = Message::Ack { msgid: 5, ack: 3 };
        let xml = String::from_utf8(msg.xml()).unwrap();
        assert!(xml.contains("type=\"ACK\""));
        assert!(xml.contains("msgid=\"5\""));
        assert!(xml.contains("ack=\"3\""));
        assert!(xml.contains("/>") || xml.contains("</message>"));
    }

    #[test]
    fn test_xml_configuration_with_payload() {
        let mut msg = Message::Configuration {
            msgid: 0,
            gateway: "gateway1".into(),
            data: None,
        };
        msg.data_encode(b"topology");
        let xml = String::from_utf8(msg.xml()).unwrap();
        assert!(xml.contains("type=\"CFG\""));
        assert!(xml.contains("gateway=\"gateway1\""));
        assert!(xml.contains("</message>"));
        // Should have base64 content between tags
        assert!(xml.contains(&base64_encode(b"topology")));
    }

    #[test]
    fn test_xml_error() {
        let msg = Message::Error {
            msgid: 1,
            reason: "something broke".into(),
        };
        let xml = String::from_utf8(msg.xml()).unwrap();
        assert!(xml.contains("type=\"ERR\""));
        assert!(xml.contains("reason=\"something broke\""));
    }

    #[test]
    fn test_xml_retcode() {
        let msg = Message::Retcode {
            msgid: 2,
            srcid: 10,
            retcode: -1,
            nodes: "node[1-5]".into(),
        };
        let xml = String::from_utf8(msg.xml()).unwrap();
        assert!(xml.contains("type=\"RET\""));
        assert!(xml.contains("retcode=\"-1\""));
        assert!(xml.contains("nodes=\"node[1-5]\""));
    }

    #[test]
    fn test_xml_control() {
        let msg = Message::Control {
            msgid: 3,
            srcid: 1,
            action: "shell".into(),
            target: "node[1-10]".into(),
            data: None,
        };
        let xml = String::from_utf8(msg.xml()).unwrap();
        assert!(xml.contains("type=\"CTL\""));
        assert!(xml.contains("action=\"shell\""));
        assert!(xml.contains("target=\"node[1-10]\""));
    }

    #[test]
    fn test_xml_routing() {
        let msg = Message::Routing {
            msgid: 4,
            srcid: 2,
            event: "connect".into(),
            gateway: "gw2".into(),
            targets: "node[20-30]".into(),
        };
        let xml = String::from_utf8(msg.xml()).unwrap();
        assert!(xml.contains("type=\"RTR\""));
        assert!(xml.contains("event=\"connect\""));
        assert!(xml.contains("gateway=\"gw2\""));
        assert!(xml.contains("targets=\"node[20-30]\""));
    }

    // -- XmlReader tests ----------------------------------------------------

    #[test]
    fn test_xmlreader_channel_start_end() {
        let mut reader = XmlReader::new();
        reader.feed("<channel version=\"1.0\"></channel>").unwrap();
        assert!(reader.msg_available());
        let m1 = reader.pop_msg().unwrap();
        assert_eq!(m1.ident(), "CHA");
        assert_eq!(reader.version.as_deref(), Some("1.0"));

        let m2 = reader.pop_msg().unwrap();
        assert_eq!(m2.ident(), "END");
        assert!(!reader.msg_available());
    }

    #[test]
    fn test_xmlreader_ack_message() {
        let mut reader = XmlReader::new();
        reader
            .feed(
                "<channel version=\"1.0\"><message type=\"ACK\" msgid=\"0\" ack=\"5\"/></channel>",
            )
            .unwrap();

        let start = reader.pop_msg().unwrap();
        assert_eq!(start.ident(), "CHA");

        let ack = reader.pop_msg().unwrap();
        assert_eq!(ack.ident(), "ACK");
        if let Message::Ack { ack, msgid, .. } = ack {
            assert_eq!(ack, 5);
            assert_eq!(msgid, 0);
        } else {
            panic!("expected Ack variant");
        }

        let end = reader.pop_msg().unwrap();
        assert_eq!(end.ident(), "END");
    }

    #[test]
    fn test_xmlreader_message_with_payload() {
        let payload_b64 = base64_encode(b"hello world");
        let xml = format!(
            "<channel version=\"2.0\"><message type=\"CFG\" msgid=\"1\" gateway=\"gw1\">{}</message></channel>",
            payload_b64
        );

        let mut reader = XmlReader::new();
        reader.feed(&xml).unwrap();

        let _start = reader.pop_msg().unwrap();
        let cfg = reader.pop_msg().unwrap();
        assert_eq!(cfg.ident(), "CFG");
        let decoded = cfg.data_decode().unwrap();
        assert_eq!(decoded, b"hello world");

        if let Message::Configuration { gateway, .. } = &cfg {
            assert_eq!(gateway, "gw1");
        } else {
            panic!("expected Configuration variant");
        }
    }

    #[test]
    fn test_xmlreader_incremental_feed() {
        let mut reader = XmlReader::new();

        // Feed in chunks
        reader.feed("<chan").unwrap();
        assert!(!reader.msg_available());
        reader.feed("nel version=\"1.0\">").unwrap();
        assert!(reader.msg_available());
        let start = reader.pop_msg().unwrap();
        assert_eq!(start.ident(), "CHA");

        reader
            .feed("<message type=\"ACK\" msgid=\"0\" ack=\"1\"/>")
            .unwrap();
        let ack = reader.pop_msg().unwrap();
        assert_eq!(ack.ident(), "ACK");

        reader.feed("</channel>").unwrap();
        let end = reader.pop_msg().unwrap();
        assert_eq!(end.ident(), "END");
    }

    #[test]
    fn test_xmlreader_unknown_type() {
        let mut reader = XmlReader::new();
        let result =
            reader.feed("<channel version=\"1\"><message type=\"BOGUS\" msgid=\"0\"/></channel>");
        assert!(result.is_err());
    }

    #[test]
    fn test_xmlreader_error_message() {
        let mut reader = XmlReader::new();
        reader
            .feed("<channel version=\"1\"><message type=\"ERR\" msgid=\"0\" reason=\"parse failed\"/></channel>")
            .unwrap();

        let _start = reader.pop_msg().unwrap();
        let err = reader.pop_msg().unwrap();
        assert_eq!(err.ident(), "ERR");
        if let Message::Error { reason, .. } = &err {
            assert_eq!(reason, "parse failed");
        } else {
            panic!("expected Error variant");
        }
    }

    #[test]
    fn test_xmlreader_retcode() {
        let mut reader = XmlReader::new();
        reader
            .feed(
                "<message type=\"RET\" msgid=\"7\" srcid=\"3\" retcode=\"0\" nodes=\"node[1-5]\"/>",
            )
            .unwrap();

        let msg = reader.pop_msg().unwrap();
        if let Message::Retcode {
            srcid,
            retcode,
            nodes,
            ..
        } = &msg
        {
            assert_eq!(*srcid, 3);
            assert_eq!(*retcode, 0);
            assert_eq!(nodes, "node[1-5]");
        } else {
            panic!("expected Retcode variant");
        }
    }

    #[test]
    fn test_xmlreader_routing() {
        let mut reader = XmlReader::new();
        reader
            .feed("<message type=\"RTR\" msgid=\"0\" srcid=\"1\" event=\"connect\" gateway=\"gw1\" targets=\"node[1-10]\"/>")
            .unwrap();

        let msg = reader.pop_msg().unwrap();
        if let Message::Routing {
            event,
            gateway,
            targets,
            ..
        } = &msg
        {
            assert_eq!(event, "connect");
            assert_eq!(gateway, "gw1");
            assert_eq!(targets, "node[1-10]");
        } else {
            panic!("expected Routing variant");
        }
    }

    #[test]
    fn test_xmlreader_stdout_stderr() {
        let payload = base64_encode(b"output line");
        let xml = format!(
            "<message type=\"OUT\" msgid=\"0\" srcid=\"5\" nodes=\"node1\">{}</message>",
            payload
        );
        let mut reader = XmlReader::new();
        reader.feed(&xml).unwrap();

        let msg = reader.pop_msg().unwrap();
        assert_eq!(msg.ident(), "OUT");
        assert_eq!(msg.srcid(), Some(5));
        assert_eq!(msg.nodes(), Some("node1"));
        assert_eq!(msg.data_decode().unwrap(), b"output line");

        // StdErr
        let xml2 = format!(
            "<message type=\"SER\" msgid=\"1\" srcid=\"6\" nodes=\"node2\">{}</message>",
            base64_encode(b"error line")
        );
        reader.feed(&xml2).unwrap();
        let msg2 = reader.pop_msg().unwrap();
        assert_eq!(msg2.ident(), "SER");
        assert_eq!(msg2.data_decode().unwrap(), b"error line");
    }

    #[test]
    fn test_xmlreader_timeout() {
        let mut reader = XmlReader::new();
        reader
            .feed("<message type=\"TIM\" msgid=\"0\" srcid=\"1\" nodes=\"node[5-8]\"/>")
            .unwrap();
        let msg = reader.pop_msg().unwrap();
        assert_eq!(msg.ident(), "TIM");
        assert_eq!(msg.nodes(), Some("node[5-8]"));
    }

    #[test]
    fn test_xmlreader_control() {
        let payload = base64_encode(b"command data");
        let xml = format!(
            "<message type=\"CTL\" msgid=\"0\" srcid=\"1\" action=\"shell\" target=\"node[1-3]\">{}</message>",
            payload
        );
        let mut reader = XmlReader::new();
        reader.feed(&xml).unwrap();

        let msg = reader.pop_msg().unwrap();
        if let Message::Control { action, target, .. } = &msg {
            assert_eq!(action, "shell");
            assert_eq!(target, "node[1-3]");
        } else {
            panic!("expected Control variant");
        }
        assert_eq!(msg.data_decode().unwrap(), b"command data");
    }

    // -- XML roundtrip tests ------------------------------------------------

    #[test]
    fn test_xml_roundtrip_ack() {
        let original = Message::Ack { msgid: 10, ack: 7 };
        let xml = String::from_utf8(original.xml()).unwrap();
        let mut reader = XmlReader::new();
        reader.feed(&xml).unwrap();
        let parsed = reader.pop_msg().unwrap();
        assert_eq!(parsed.ident(), "ACK");
        if let Message::Ack { msgid, ack, .. } = parsed {
            assert_eq!(msgid, 10);
            assert_eq!(ack, 7);
        }
    }

    #[test]
    fn test_xml_roundtrip_cfg_with_data() {
        let mut original = Message::Configuration {
            msgid: 0,
            gateway: "my-gateway".into(),
            data: None,
        };
        original.data_encode(b"topology bytes here");
        let xml = String::from_utf8(original.xml()).unwrap();

        let mut reader = XmlReader::new();
        reader.feed(&xml).unwrap();
        let parsed = reader.pop_msg().unwrap();
        assert_eq!(parsed.ident(), "CFG");
        if let Message::Configuration { gateway, .. } = &parsed {
            assert_eq!(gateway, "my-gateway");
        }
        assert_eq!(parsed.data_decode().unwrap(), b"topology bytes here");
    }

    #[test]
    fn test_xml_roundtrip_error() {
        let original = Message::Error {
            msgid: 3,
            reason: "test <error> & reason".into(),
        };
        let xml = String::from_utf8(original.xml()).unwrap();
        let mut reader = XmlReader::new();
        reader.feed(&xml).unwrap();
        let parsed = reader.pop_msg().unwrap();
        if let Message::Error { reason, .. } = &parsed {
            assert_eq!(reason, "test <error> & reason");
        } else {
            panic!("expected Error variant");
        }
    }

    #[test]
    fn test_xml_roundtrip_retcode() {
        let original = Message::Retcode {
            msgid: 5,
            srcid: 2,
            retcode: 127,
            nodes: "node[1-100]".into(),
        };
        let xml = String::from_utf8(original.xml()).unwrap();
        let mut reader = XmlReader::new();
        reader.feed(&xml).unwrap();
        let parsed = reader.pop_msg().unwrap();
        if let Message::Retcode { retcode, nodes, .. } = &parsed {
            assert_eq!(*retcode, 127);
            assert_eq!(nodes, "node[1-100]");
        } else {
            panic!("expected Retcode variant");
        }
    }

    // -- Channel tests ------------------------------------------------------

    #[test]
    fn test_channel_creation() {
        let ch = Channel::new(true);
        assert!(!ch.opened);
        assert!(!ch.setup);
        assert!(ch.initiator);
    }

    #[test]
    fn test_channel_init_xml() {
        let ch = Channel::new(false);
        let xml = ch.init_xml();
        assert_eq!(
            String::from_utf8(xml).unwrap(),
            "<?xml version='1.0' encoding='UTF-8'?>"
        );
    }

    #[test]
    fn test_channel_open_close_xml() {
        let ch = Channel::new(true);
        let open = String::from_utf8(ch.open_xml("3.0")).unwrap();
        assert_eq!(open, "<channel version=\"3.0\">");
        let close = String::from_utf8(ch.close_xml()).unwrap();
        assert_eq!(close, "</channel>");
    }

    #[test]
    fn test_channel_send() {
        let mut ch = Channel::new(true);
        let msg = Message::ack(42);
        ch.send(&msg);
        let outgoing = ch.take_outgoing().unwrap();
        let xml = String::from_utf8(outgoing).unwrap();
        assert!(xml.contains("type=\"ACK\""));
        assert!(xml.ends_with('\n'));
    }

    #[test]
    fn test_channel_feed_and_receive() {
        let mut ch = Channel::new(false);
        let messages = ch
            .feed(
                "<channel version=\"1.0\"><message type=\"ACK\" msgid=\"0\" ack=\"1\"/></channel>",
            )
            .unwrap();
        assert_eq!(messages.len(), 3); // Start, ACK, End
        assert_eq!(messages[0].ident(), "CHA");
        assert_eq!(messages[1].ident(), "ACK");
        assert_eq!(messages[2].ident(), "END");
    }

    #[test]
    fn test_channel_outbox_ordering() {
        let mut ch = Channel::new(true);
        ch.send(&Message::ack(1));
        ch.send(&Message::ack(2));
        ch.send(&Message::ack(3));

        let m1 = ch.take_outgoing().unwrap();
        let m2 = ch.take_outgoing().unwrap();
        let m3 = ch.take_outgoing().unwrap();
        assert!(ch.take_outgoing().is_none());

        // They should come out in order
        assert!(String::from_utf8(m1).unwrap().contains("ACK"));
        assert!(String::from_utf8(m2).unwrap().contains("ACK"));
        assert!(String::from_utf8(m3).unwrap().contains("ACK"));
    }

    // -- Message Display test -----------------------------------------------

    #[test]
    fn test_message_display() {
        let msg = Message::ack(0);
        let display = format!("{}", msg);
        assert!(display.contains("ACK"));
    }

    // -- Edge case tests ----------------------------------------------------

    #[test]
    fn test_from_attrs_missing_type() {
        let attrs = HashMap::new();
        assert!(Message::from_attrs(&attrs).is_err());
    }

    #[test]
    fn test_from_attrs_missing_srcid() {
        let mut attrs = HashMap::new();
        attrs.insert("type".into(), "CTL".into());
        attrs.insert("msgid".into(), "0".into());
        // missing srcid
        assert!(Message::from_attrs(&attrs).is_err());
    }

    #[test]
    fn test_data_decode_no_data() {
        let msg = Message::configuration("gw");
        let result = msg.data_decode();
        assert!(result.is_err());
    }

    #[test]
    fn test_xml_special_characters_roundtrip() {
        let msg = Message::Error {
            msgid: 0,
            reason: "node<1>&\"2\"".into(),
        };
        let xml = String::from_utf8(msg.xml()).unwrap();
        // Should be escaped in XML
        assert!(xml.contains("&lt;"));
        assert!(xml.contains("&amp;"));

        // Parse it back
        let mut reader = XmlReader::new();
        reader.feed(&xml).unwrap();
        let parsed = reader.pop_msg().unwrap();
        if let Message::Error { reason, .. } = &parsed {
            assert_eq!(reason, "node<1>&\"2\"");
        }
    }
}
