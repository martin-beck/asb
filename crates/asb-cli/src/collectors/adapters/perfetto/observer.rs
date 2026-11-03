//! Content-free observation grammar shared by proxy and tool producers.
//!
//! The transport is deliberately separate from tracefs.  Child processes can
//! submit only this fixed grammar; the collector retains the pinned marker and
//! decides whether an observation is accepted and written.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EventKind {
    Begin,
    End,
    Instant,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Event {
    pub kind: EventKind,
    pub subject: &'static str,
    pub cookie: u64,
    pub reason: Option<&'static str>,
}

const MAX_LINE: usize = 128;

/// Parse one strict, content-free producer message.
pub(super) fn parse(input: &[u8]) -> Result<Event, &'static str> {
    if input.is_empty() || input.len() > MAX_LINE || !input.is_ascii() {
        return Err("malformed");
    }
    let text = std::str::from_utf8(input)
        .map_err(|_| "malformed")?
        .trim_end_matches('\n');
    let parts = text.split(' ').collect::<Vec<_>>();
    let kind = match parts.first().copied() {
        Some("begin") => EventKind::Begin,
        Some("end") => EventKind::End,
        Some("instant") => EventKind::Instant,
        _ => return Err("malformed"),
    };
    let subject = match parts.get(1).copied() {
        Some("llm_request") => "llm_request",
        Some("tool") => "tool",
        _ => return Err("subject"),
    };
    let cookie = parts
        .get(2)
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .ok_or("cookie")?;
    let reason = match (kind, parts.get(3).copied(), parts.len()) {
        (EventKind::Instant, Some("http_error"), 4) => Some("http_error"),
        (EventKind::Instant, Some("cancelled"), 4) => Some("cancelled"),
        (EventKind::Instant, Some("error"), 4) => Some("error"),
        (EventKind::Instant, _, _) => return Err("reason"),
        (_, None, 3) => None,
        _ => return Err("arity"),
    };
    Ok(Event {
        kind,
        subject,
        cookie,
        reason,
    })
}

/// Render one marker after collector-side validation. The name contains only
/// ASB-controlled labels, a trace identity and an opaque numeric cookie.
pub(super) fn marker(event: &Event, pid: u32, trace_id: &str) -> String {
    let subject = match event.subject {
        "llm_request" => "asb.llm.request",
        "tool" => "asb.tool",
        _ => unreachable!("parser owns subjects"),
    };
    let name = match event.reason {
        Some(reason) => format!("{subject}.{reason}.{trace_id}"),
        None => format!("{subject}.{trace_id}"),
    };
    match event.kind {
        EventKind::Begin => format!("S|{pid}|{name}|{}", event.cookie),
        EventKind::End => format!("F|{pid}|{name}|{}", event.cookie),
        EventKind::Instant => format!("I|{pid}|{name}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_fixed_content_free_observations() {
        let event = parse(b"begin llm_request 7\n").unwrap();
        assert_eq!(event.kind, EventKind::Begin);
        assert_eq!(marker(&event, 42, "42-0"), "S|42|asb.llm.request.42-0|7");
        let failure = parse(b"instant tool 3 http_error\n").unwrap();
        assert_eq!(
            marker(&failure, 42, "42-0"),
            "I|42|asb.tool.http_error.42-0"
        );
    }

    #[test]
    fn rejects_content_and_unbounded_or_malformed_input() {
        for input in [
            b"begin llm_request 0\n".as_slice(),
            b"begin llm_request 1 https://provider.example\n",
            b"instant llm_request 1 prompt\n",
            b"begin tool 1\0\n",
        ] {
            assert!(parse(input).is_err(), "{input:?}");
        }
    }
}
