//! Shared configuration extensions require a product path or a bounded content marker.
use super::SourceLanguage;

const DETECTION_BYTES: usize = 8_192;

pub(super) fn candidate(path: &str) -> bool {
    path.rsplit_once('.').is_some_and(|(_, extension)| {
        ["cfg", "conf", "ini"]
            .iter()
            .any(|value| extension.eq_ignore_ascii_case(value))
    })
}

pub(super) fn detect(path: &str, source: Option<&str>) -> Option<SourceLanguage> {
    let path = path.replace('\\', "/").to_ascii_lowercase();
    let filename = path.rsplit('/').next()?;
    let extension = filename.rsplit_once('.')?.1;
    let in_directory = |name| path.split('/').any(|part| part == name);
    if matches!(extension, "conf" | "ini")
        && (filename.starts_with("rtpengine.")
            || filename.starts_with("rtpengine-")
            || in_directory("rtpengine"))
    {
        return Some(SourceLanguage::RtpEngine);
    }
    if matches!(extension, "cfg" | "conf")
        && (filename.starts_with("kamailio.")
            || filename.starts_with("kamailio-")
            || in_directory("kamailio"))
    {
        return Some(SourceLanguage::Kamailio);
    }
    let source = source?;
    let prefix = &source[..source.floor_char_boundary(source.len().min(DETECTION_BYTES))];
    if matches!(extension, "cfg" | "conf")
        && prefix.lines().any(|line| {
            let line = line.trim();
            line == "#!KAMAILIO"
                || [
                    "request_route",
                    "route",
                    "failure_route",
                    "onreply_route",
                    "branch_route",
                    "event_route",
                    "onsend_route",
                    "reply_route",
                    "branch_failure_route",
                ]
                .iter()
                .any(|keyword| {
                    line.strip_prefix(keyword)
                        .is_some_and(|tail| tail.trim_start().starts_with(['[', '{']))
                })
        })
    {
        return Some(SourceLanguage::Kamailio);
    }
    if matches!(extension, "conf" | "ini")
        && prefix.lines().any(|line| line.trim() == "[rtpengine]")
    {
        return Some(SourceLanguage::RtpEngine);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ambiguous_extensions_need_explicit_telephony_evidence() {
        for path in [
            "etc/kamailio/kamailio.cfg",
            "KAMAILIO/ROUTES.CONF",
            "etc\\kamailio\\auth.cfg",
        ] {
            assert_eq!(
                SourceLanguage::detect(path, None),
                Some(SourceLanguage::Kamailio)
            );
        }
        for path in [
            "etc/rtpengine/rtpengine.conf",
            "rtpengine/alternate.ini",
            "RTPENGINE-TEST.CONF",
        ] {
            assert_eq!(
                SourceLanguage::detect(path, None),
                Some(SourceLanguage::RtpEngine)
            );
        }
        for path in ["other.cfg", "nginx.conf", "settings.ini"] {
            assert!(SourceLanguage::is_native_candidate_path(path));
            assert_eq!(SourceLanguage::detect(path, None), None);
            assert_eq!(SourceLanguage::detect(path, Some("key=value\n")), None);
            assert!(!SourceLanguage::is_v1_candidate_path(path));
        }
        assert_eq!(
            SourceLanguage::detect("edge.cfg", Some("#!KAMAILIO\nrequest_route {}")),
            Some(SourceLanguage::Kamailio)
        );
        assert_eq!(
            SourceLanguage::detect("media.ini", Some("[rtpengine]\ninterface=lo")),
            Some(SourceLanguage::RtpEngine)
        );
        assert_eq!(
            SourceLanguage::detect("src/settings.xml", None),
            Some(SourceLanguage::Xml)
        );
        assert_eq!(
            SourceLanguage::detect("auth.inc", None),
            Some(SourceLanguage::Php)
        );
    }
}
