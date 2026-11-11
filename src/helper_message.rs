/// A line the PAM helper writes to its stdout.
#[derive(Debug, PartialEq)]
pub enum HelperMessage {
    /// PAM waits for a password on the helper's stdin.
    Prompt,
    Info(String),
    Error(String),
    Success,
    Failure,
}

impl HelperMessage {
    pub fn encode(&self) -> String {
        let single_line = |text: &str| text.replace(['\n', '\r'], " ");
        match self {
            HelperMessage::Prompt => String::from("prompt"),
            HelperMessage::Info(text) => format!("info {}", single_line(text)),
            HelperMessage::Error(text) => format!("error {}", single_line(text)),
            HelperMessage::Success => String::from("success"),
            HelperMessage::Failure => String::from("failure"),
        }
    }

    pub fn decode(line: &str) -> Option<Self> {
        let (kind, text) = line.split_once(' ').unwrap_or((line, ""));
        match kind {
            "prompt" => Some(HelperMessage::Prompt),
            "info" => Some(HelperMessage::Info(text.to_owned())),
            "error" => Some(HelperMessage::Error(text.to_owned())),
            "success" => Some(HelperMessage::Success),
            "failure" => Some(HelperMessage::Failure),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let messages = [
            HelperMessage::Prompt,
            HelperMessage::Info(String::from("Place your finger")),
            HelperMessage::Error(String::from("")),
            HelperMessage::Success,
            HelperMessage::Failure,
        ];
        for message in messages {
            assert_eq!(HelperMessage::decode(&message.encode()), Some(message));
        }
    }

    #[test]
    fn keeps_texts_on_one_line() {
        let encoded = HelperMessage::Error(String::from("a\nb")).encode();

        assert_eq!(encoded, "error a b");
    }

    #[test]
    fn rejects_unknown_lines() {
        assert_eq!(HelperMessage::decode("unlock"), None);
    }
}
