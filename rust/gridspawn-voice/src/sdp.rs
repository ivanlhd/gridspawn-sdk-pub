//! The bridge between mediasoup's transport parameters and SDP, the way
//! mediasoup-client builds its remote descriptions: the SFU is ICE-lite and
//! this client is always the DTLS client.

use serde_json::{json, Map, Value};

use crate::error::VoiceError;

/// One mediasoup WebRTC transport, as `createTransport` answered.
#[derive(Clone, Debug)]
pub(crate) struct TransportInfo {
    pub id: String,
    ufrag: String,
    pwd: String,
    fingerprint: (String, String),
    candidates: Vec<String>,
}

impl TransportInfo {
    pub fn parse(value: &Value) -> Result<Self, VoiceError> {
        let invalid = VoiceError::InvalidResponse;
        let id = value["id"]
            .as_str()
            .ok_or(invalid("transport id"))?
            .to_owned();
        let ice = &value["iceParameters"];
        let ufrag = ice["usernameFragment"]
            .as_str()
            .ok_or(invalid("ice ufrag"))?;
        let pwd = ice["password"].as_str().ok_or(invalid("ice password"))?;
        let fingerprint = value["dtlsParameters"]["fingerprints"]
            .as_array()
            .and_then(|list| list.iter().find(|f| f["algorithm"] == "sha-256"))
            .ok_or(invalid("no sha-256 fingerprint"))?;
        let candidates = value["iceCandidates"]
            .as_array()
            .ok_or(invalid("ice candidates"))?
            .iter()
            .map(candidate_line)
            .collect::<Result<Vec<_>, _>>()?;
        if candidates.is_empty() {
            return Err(invalid("no ice candidates"));
        }
        Ok(Self {
            id,
            ufrag: ufrag.to_owned(),
            pwd: pwd.to_owned(),
            fingerprint: (
                "sha-256".to_owned(),
                fingerprint["value"]
                    .as_str()
                    .ok_or(invalid("fingerprint"))?
                    .to_owned(),
            ),
            candidates,
        })
    }
}

fn candidate_line(candidate: &Value) -> Result<String, VoiceError> {
    let ip = candidate["ip"]
        .as_str()
        .or(candidate["address"].as_str())
        .ok_or(VoiceError::InvalidResponse("candidate address"))?;
    let protocol = candidate["protocol"].as_str().unwrap_or("udp");
    let mut line = format!(
        "a=candidate:{} 1 {protocol} {} {ip} {} typ {}",
        candidate["foundation"].as_str().unwrap_or("0"),
        candidate["priority"].as_u64().unwrap_or(1),
        candidate["port"]
            .as_u64()
            .ok_or(VoiceError::InvalidResponse("candidate port"))?,
        candidate["type"].as_str().unwrap_or("host"),
    );
    if protocol == "tcp" {
        line.push_str(&format!(
            " tcptype {}",
            candidate["tcpType"].as_str().unwrap_or("passive")
        ));
    }
    Ok(line)
}

/// The Opus codec as both sides describe it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OpusCodec {
    pub payload_type: u8,
    /// `a=fmtp` parameters, e.g. `minptime=10;useinbandfec=1`.
    pub fmtp: String,
}

impl OpusCodec {
    /// From a consumer's `rtpParameters.codecs[0]`.
    pub fn from_rtp_codec(codec: &Value) -> Result<Self, VoiceError> {
        let payload_type = codec["payloadType"]
            .as_u64()
            .filter(|pt| *pt < 128)
            .ok_or(VoiceError::InvalidResponse("payload type"))? as u8;
        if !codec["mimeType"]
            .as_str()
            .is_some_and(|mime| mime.eq_ignore_ascii_case("audio/opus"))
        {
            return Err(VoiceError::InvalidResponse("consumer codec is not Opus"));
        }
        let fmtp = codec["parameters"]
            .as_object()
            .map(fmtp_line)
            .unwrap_or_default();
        Ok(Self { payload_type, fmtp })
    }
}

fn fmtp_line(parameters: &Map<String, Value>) -> String {
    parameters
        .iter()
        .map(|(key, value)| match value {
            Value::String(text) => format!("{key}={text}"),
            other => format!("{key}={other}"),
        })
        .collect::<Vec<_>>()
        .join(";")
}

fn session_header(mids: &[&str]) -> String {
    format!(
        "v=0\r\no=mediasoup 1 1 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\na=ice-lite\r\n\
         a=group:BUNDLE {}\r\na=msid-semantic: WMS *\r\n",
        mids.join(" ")
    )
}

fn media_section(
    transport: &TransportInfo,
    mid: &str,
    direction: &str,
    setup: &str,
    codec: &OpusCodec,
    extra: &str,
) -> String {
    let mut sdp = format!(
        "m=audio 7 UDP/TLS/RTP/SAVPF {pt}\r\nc=IN IP4 127.0.0.1\r\n\
         a=ice-ufrag:{ufrag}\r\na=ice-pwd:{pwd}\r\n\
         a=fingerprint:{alg} {fp}\r\na=setup:{setup}\r\n\
         a=mid:{mid}\r\na={direction}\r\na=rtcp-mux\r\na=rtcp-rsize\r\n\
         a=rtpmap:{pt} opus/48000/2\r\n",
        pt = codec.payload_type,
        ufrag = transport.ufrag,
        pwd = transport.pwd,
        alg = transport.fingerprint.0,
        fp = transport.fingerprint.1,
    );
    if !codec.fmtp.is_empty() {
        sdp.push_str(&format!("a=fmtp:{} {}\r\n", codec.payload_type, codec.fmtp));
    }
    sdp.push_str(extra);
    for line in &transport.candidates {
        sdp.push_str(line);
        sdp.push_str("\r\n");
    }
    sdp.push_str("a=end-of-candidates\r\n");
    sdp
}

/// The SFU's answer to our send offer: it receives our one audio section.
pub(crate) fn send_answer(transport: &TransportInfo, mid: &str, codec: &OpusCodec) -> String {
    session_header(&[mid]) + &media_section(transport, mid, "recvonly", "passive", codec, "")
}

/// One consumer in the receive offer.
#[derive(Clone, Debug)]
pub(crate) struct RecvSection {
    pub mid: String,
    pub codec: OpusCodec,
    pub ssrc: u32,
    pub cname: String,
    /// Closed consumers stay as inactive sections, so mids are never reused.
    pub active: bool,
}

/// The SFU's offer to our receive transport: one section per consumer.
///
/// The first section carries the BUNDLE's transport, as in mediasoup-client:
/// when its consumer closes it is disabled (inactive, still bundled, with the
/// ICE and DTLS parameters), never rejected. Otherwise an offer whose
/// consumers have all closed would have no transport at all, and webrtc-rs
/// refuses it ("no ice-ufrag"). Later closed sections are rejected (port 0).
pub(crate) fn recv_offer(transport: &TransportInfo, sections: &[RecvSection]) -> String {
    let mids: Vec<&str> = sections
        .iter()
        .enumerate()
        .filter(|(index, s)| s.active || *index == 0)
        .map(|(_, s)| s.mid.as_str())
        .collect();
    let mut sdp = session_header(&mids);
    for (index, section) in sections.iter().enumerate() {
        if !section.active && index == 0 {
            sdp.push_str(&media_section(
                transport,
                &section.mid,
                "inactive",
                "actpass",
                &section.codec,
                "",
            ));
        } else if section.active {
            let extra = format!(
                "a=msid:gridspawn-{mid} audio-{mid}\r\na=ssrc:{ssrc} cname:{cname}\r\n",
                mid = section.mid,
                ssrc = section.ssrc,
                cname = section.cname
            );
            sdp.push_str(&media_section(
                transport,
                &section.mid,
                "sendonly",
                "actpass",
                &section.codec,
                &extra,
            ));
        } else {
            sdp.push_str(&format!(
                "m=audio 0 UDP/TLS/RTP/SAVPF {pt}\r\nc=IN IP4 127.0.0.1\r\na=mid:{mid}\r\na=inactive\r\n\
                 a=rtpmap:{pt} opus/48000/2\r\n",
                pt = section.codec.payload_type,
                mid = section.mid
            ));
        }
    }
    sdp
}

/// Our DTLS parameters, from a local description: always the DTLS client.
pub(crate) fn local_dtls(sdp: &str) -> Result<Value, VoiceError> {
    let fingerprint = attribute(sdp, "a=fingerprint:")
        .ok_or(VoiceError::InvalidResponse("no local fingerprint"))?;
    let (algorithm, value) = fingerprint
        .split_once(' ')
        .ok_or(VoiceError::InvalidResponse("bad local fingerprint"))?;
    Ok(json!({ "role": "client", "fingerprints": [{ "algorithm": algorithm, "value": value }] }))
}

pub(crate) fn attribute<'a>(sdp: &'a str, prefix: &str) -> Option<&'a str> {
    sdp.lines()
        .find_map(|line| line.trim_end().strip_prefix(prefix))
}

/// The CNAME our sender announced.
pub(crate) fn local_cname(sdp: &str) -> Option<String> {
    sdp.lines().find_map(|line| {
        line.split_once(" cname:")
            .map(|(_, cname)| cname.trim().to_owned())
    })
}

/// A mediasoup-like transport for tests.
#[cfg(test)]
pub(crate) fn test_transport() -> TransportInfo {
    TransportInfo::parse(&json!({
        "id": "t1",
        "iceParameters": { "usernameFragment": "uf", "password": "pw", "iceLite": true },
        "iceCandidates": [
            { "foundation": "udpcandidate", "priority": 1076302079u64, "ip": "203.0.113.5", "protocol": "udp", "port": 40001, "type": "host" },
            { "foundation": "tcpcandidate", "priority": 1076276479u64, "address": "203.0.113.5", "protocol": "tcp", "port": 40002, "type": "host", "tcpType": "passive" }
        ],
        "dtlsParameters": { "role": "auto", "fingerprints": [
            { "algorithm": "sha-1", "value": "AA" },
            { "algorithm": "sha-256", "value": "BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA" } ] }
    }))
    .unwrap()
}

/// The router's Opus as a consumer reports it, for tests.
#[cfg(test)]
pub(crate) fn test_opus() -> OpusCodec {
    OpusCodec::from_rtp_codec(&json!({
        "mimeType": "audio/opus", "payloadType": 100, "clockRate": 48000, "channels": 2,
        "parameters": { "minptime": 10, "useinbandfec": 1 }
    }))
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transport() -> TransportInfo {
        test_transport()
    }

    fn opus() -> OpusCodec {
        test_opus()
    }

    #[test]
    fn transport_and_codec() {
        let t = transport();
        assert!(t.fingerprint.1.starts_with("BB:CC:DD"));
        assert_eq!(t.candidates.len(), 2);
        assert!(t.candidates[1].ends_with("tcptype passive"));
        assert_eq!(opus().fmtp, "minptime=10;useinbandfec=1");
        assert!(
            OpusCodec::from_rtp_codec(&json!({"mimeType": "audio/PCMU", "payloadType": 0}))
                .is_err()
        );
    }

    #[test]
    fn send_answer_shape() {
        let sdp = send_answer(
            &transport(),
            "0",
            &OpusCodec {
                payload_type: 111,
                fmtp: "minptime=10;useinbandfec=1".into(),
            },
        );
        for line in [
            "a=ice-lite",
            "a=group:BUNDLE 0",
            "a=setup:passive",
            "a=recvonly",
            "a=fingerprint:sha-256 BB:CC",
            "a=rtpmap:111 opus/48000/2",
            "a=fmtp:111 minptime=10;useinbandfec=1",
            "a=end-of-candidates",
        ] {
            assert!(sdp.contains(line), "{line}");
        }
        assert!(sdp.contains("\r\n"));
    }

    fn section(mid: &str, ssrc: u32, active: bool) -> RecvSection {
        RecvSection {
            mid: mid.into(),
            codec: opus(),
            ssrc,
            cname: format!("c{ssrc}"),
            active,
        }
    }

    /// The text of one media section (from its m= line to the next).
    fn media<'a>(sdp: &'a str, mid: &str) -> &'a str {
        let at = sdp.find(&format!("a=mid:{mid}\r\n")).unwrap();
        let start = sdp[..at].rfind("m=audio").unwrap();
        let end = sdp[at..].find("m=audio").map_or(sdp.len(), |n| at + n);
        &sdp[start..end]
    }

    #[test]
    fn recv_offer_keeps_closed_sections() {
        // mid 0 closed (disabled, still bundled), mid 1 closed (rejected), mid 2 open.
        let sdp = recv_offer(
            &transport(),
            &[
                section("0", 11, false),
                section("1", 22, false),
                section("2", 33, true),
            ],
        );
        assert!(sdp.contains("a=group:BUNDLE 0 2\r\n"));
        let first = media(&sdp, "0");
        assert!(first.starts_with("m=audio 7 "), "{first}");
        assert!(first.contains("a=inactive") && first.contains("a=ice-ufrag:uf"));
        assert!(!first.contains("a=ssrc:"));
        let rejected = media(&sdp, "1");
        assert!(rejected.starts_with("m=audio 0 "), "{rejected}");
        assert!(rejected.contains("a=inactive") && !rejected.contains("a=ice-ufrag"));
        let open = media(&sdp, "2");
        assert!(open.contains("a=sendonly") && open.contains("a=ssrc:33 cname:c33"));
        assert!(open.contains("a=setup:actpass"));
        assert!(!sdp.contains("a=ssrc:11") && !sdp.contains("a=ssrc:22"));
    }

    /// Regression: when the only consumer closes, the offer still carries the
    /// transport (webrtc-rs refused the old one with "no ice-ufrag").
    #[test]
    fn recv_offer_with_every_consumer_closed_keeps_the_transport() {
        let sdp = recv_offer(&transport(), &[section("0", 11, false)]);
        assert!(sdp.contains("a=group:BUNDLE 0\r\n"));
        assert!(sdp.contains("a=ice-ufrag:uf") && sdp.contains("a=fingerprint:sha-256 BB:CC"));
        assert!(sdp.contains("a=inactive") && sdp.contains("a=end-of-candidates"));
        // And a consumer added after it is bundled with it.
        let next = recv_offer(
            &transport(),
            &[section("0", 11, false), section("1", 22, true)],
        );
        assert!(next.contains("a=group:BUNDLE 0 1\r\n"));
    }

    #[test]
    fn local_parameters() {
        let local = "v=0\r\na=fingerprint:sha-256 DE:AD\r\na=mid:0\r\na=ssrc:1 cname:xyz\r\n";
        assert_eq!(
            local_dtls(local).unwrap()["fingerprints"][0]["value"],
            "DE:AD"
        );
        assert_eq!(local_dtls(local).unwrap()["role"], "client");
        assert_eq!(attribute(local, "a=mid:"), Some("0"));
        assert_eq!(local_cname(local).as_deref(), Some("xyz"));
    }
}
