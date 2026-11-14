// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah

//! Timestamp scripts for placing stills and video clips on a timeline.
//!
//! One row is \`HH:MM:SS - HH:MM:SS | media label\`. Labels may be a
//! bin item's name/path, or a one-based ordinal such as \`Image 3\`.

use thiserror::Error;

use crate::model::{MediaKind, Project};

/// Maximum number of placements accepted in one pasted script.
const MAX_SCRIPT_ROWS: usize = 1_000;

/// Automatic picture movement applied while a timestamp script is placed.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AutoMotion {
    /// Leave the picture still.
    #[default]
    None,
    /// Move from a slightly wider view to a closer view.
    ZoomIn,
    /// Move from a closer view to the original framing.
    ZoomOut,
    /// Pan a still vertically from its top toward its bottom.
    VerticalScroll,
}

/// One parsed line of a timestamp script.
#[derive(Clone, Debug, PartialEq)]
pub struct ScriptEntry {
    /// The line in the pasted script, counted from one.
    pub line: usize,
    /// Timeline start in seconds.
    pub start: f64,
    /// Timeline end in seconds.
    pub end: f64,
    /// The media-bin name, path or ordinal label.
    pub label: String,
}

/// One resolved media placement. The speed is 1.0 for a trim or an image
/// hold, and below 1.0 when a short video must be slowed to fill the span.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimedMediaPlacement {
    /// Id of the media-bin item resolved from the script label.
    pub media_id: String,
    /// Start of the clip on the active timeline, in seconds.
    pub start: f64,
    /// Time occupied by the clip on the timeline, in seconds.
    pub duration: f64,
    /// Playback multiplier: 1.0 is normal speed; values below 1.0 slow it down.
    pub speed: f64,
    /// Movement to apply to the placed picture.
    pub motion: AutoMotion,
}

/// A script line or media reference could not be used.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("line {line}: {reason}")]
pub struct TimestampScriptError {
    /// One-based line number in the pasted script.
    pub line: usize,
    /// Human-readable reason the line or script was rejected.
    pub reason: String,
}

/// Parse timestamp lines such as \`00:00:00 - 00:00:05 | Image 1\`.
/// Blank lines and lines beginning with \`#\` are ignored. Both a hyphen
/// separator and SubRip's \`-->\` separator are accepted.
pub fn parse_script(text: &str) -> Result<Vec<ScriptEntry>, TimestampScriptError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut entries = Vec::new();

    for (index, raw) in text.lines().enumerate() {
        let line_number = index + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if entries.len() >= MAX_SCRIPT_ROWS {
            return Err(error(
                line_number,
                "a timestamp script cannot contain more than 1,000 placements",
            ));
        }

        let (timing, label) = line
            .split_once('|')
            .ok_or_else(|| error(line_number, "expected 'start - end | media name'"))?;
        let label = label.trim();
        if label.is_empty() {
            return Err(error(line_number, "media name is empty"));
        }

        let (start_text, end_text) = split_range(timing).ok_or_else(|| {
            error(
                line_number,
                "expected a start and end timestamp separated by '-' or '-->'",
            )
        })?;
        let start = parse_time(start_text.trim()).ok_or_else(|| {
            error(
                line_number,
                "invalid start time; use HH:MM:SS or HH:MM:SS.mmm",
            )
        })?;
        let end = parse_time(end_text.trim()).ok_or_else(|| {
            error(
                line_number,
                "invalid end time; use HH:MM:SS or HH:MM:SS.mmm",
            )
        })?;
        if end <= start {
            return Err(error(line_number, "end time must be later than start time"));
        }
        if end - start < 1.0 / 60.0 {
            return Err(error(
                line_number,
                "duration must be at least one frame (1/60 second)",
            ));
        }
        if end > 360_000.0 {
            return Err(error(
                line_number,
                "timestamp exceeds the 100-hour project limit",
            ));
        }

        entries.push(ScriptEntry {
            line: line_number,
            start,
            end,
            label: label.to_owned(),
        });
    }

    if entries.is_empty() {
        return Err(error(1, "the timestamp script contains no usable rows"));
    }
    Ok(entries)
}

/// Resolve labels against the current media bin and calculate an exact
/// duration/speed plan before a command mutates the timeline.
pub fn resolve(
    entries: &[ScriptEntry],
    project: &Project,
    motion: AutoMotion,
) -> Result<Vec<TimedMediaPlacement>, TimestampScriptError> {
    entries
        .iter()
        .map(|entry| {
            let media = find_media(&entry.label, project).ok_or_else(|| {
                error(
                    entry.line,
                    &format!("no media item matches '{}'", entry.label),
                )
            })?;
            if media.kind == MediaKind::Audio {
                return Err(error(
                    entry.line,
                    "audio-only media cannot be placed as a picture",
                ));
            }

            let duration = entry.end - entry.start;
            let speed = match media.kind {
                MediaKind::Image => 1.0,
                MediaKind::Video => {
                    let source = media
                        .duration
                        .filter(|value| value.is_finite() && *value > 0.0)
                        .ok_or_else(|| {
                            error(
                                entry.line,
                                "video duration is unavailable; re-import the media so Concat can probe its duration",
                            )
                        })?;
                    if source < duration {
                        source / duration
                    } else {
                        1.0
                    }
                }
                MediaKind::Audio => unreachable!(),
            };

            if !(0.0625..=16.0).contains(&speed) {
                return Err(error(
                    entry.line,
                    "this video is too short to stretch to the requested duration (maximum slow-down is 16x)",
                ));
            }

            Ok(TimedMediaPlacement {
                media_id: media.id.clone(),
                start: entry.start,
                duration,
                speed,
                motion: if media.kind == MediaKind::Image {
                    motion
                } else {
                    match motion {
                        AutoMotion::ZoomIn | AutoMotion::ZoomOut => motion,
                        _ => AutoMotion::None,
                    }
                },
            })
        })
        .collect()
}

fn find_media<'a>(label: &str, project: &'a Project) -> Option<&'a crate::model::MediaItem> {
    let label = label.trim();

    // Human-friendly ordinal labels map to the media-bin order.
    if let Some((kind, ordinal)) = ordinal(label) {
        return project
            .media
            .iter()
            .filter(|item| item.kind == kind)
            .nth(ordinal - 1);
    }

    let wanted = label.to_ascii_lowercase();
    let matches: Vec<_> = project
        .media
        .iter()
        .filter(|item| {
            let name = item.name.to_ascii_lowercase();
            let path_name = basename(&item.path).to_ascii_lowercase();
            let path_stem = path_name
                .rsplit_once('.')
                .map(|(stem, _)| stem)
                .unwrap_or(&path_name);
            name == wanted || path_name == wanted || path_stem == wanted
        })
        .collect();

    if matches.len() == 1 {
        matches.first().copied()
    } else {
        None
    }
}

fn ordinal(label: &str) -> Option<(MediaKind, usize)> {
    let lower = label.trim().to_ascii_lowercase();
    let (kind, digits) = if let Some(rest) = lower.strip_prefix("image ") {
        (MediaKind::Image, rest.trim())
    } else if let Some(rest) = lower.strip_prefix("video ") {
        (MediaKind::Video, rest.trim())
    } else {
        return None;
    };
    let number = digits.parse::<usize>().ok()?;
    (number > 0).then_some((kind, number))
}

fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn split_range(text: &str) -> Option<(&str, &str)> {
    text.split_once("-->")
        .or_else(|| text.split_once('–'))
        .or_else(|| text.split_once(" - "))
}

fn parse_time(text: &str) -> Option<f64> {
    let (clock, fraction) = match text.split_once(['.', ',']) {
        Some((clock, fraction))
            if !fraction.is_empty()
                && fraction.len() <= 3
                && fraction.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            (clock, Some(fraction))
        }
        Some(_) => return None,
        None => (text, None),
    };
    let mut parts = clock.split(':');
    let hours = parts.next()?.parse::<u64>().ok()?;
    let minutes = parts.next()?.parse::<u64>().ok()?;
    let seconds = parts.next()?.parse::<u64>().ok()?;
    if parts.next().is_some() || minutes >= 60 || seconds >= 60 {
        return None;
    }
    let millis = fraction
        .and_then(|value| {
            value
                .parse::<u64>()
                .ok()
                .map(|parsed| parsed * 10u64.pow((3 - value.len()) as u32))
        })
        .unwrap_or(0);
    let total = hours
        .checked_mul(3600)?
        .checked_add(minutes * 60 + seconds)?;
    Some(total as f64 + millis as f64 / 1000.0)
}

fn error(line: usize, reason: &str) -> TimestampScriptError {
    TimestampScriptError {
        line,
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::model::{MediaItem, Timeline, Track, VideoSettings};

    fn project() -> Project {
        let mut project = Project::new();
        project.media = vec![
            MediaItem {
                id: "m1".into(),
                name: "portrait.png".into(),
                path: "/media/portrait.png".into(),
                kind: MediaKind::Image,
                ..MediaItem::default()
            },
            MediaItem {
                id: "m2".into(),
                name: "short.mp4".into(),
                path: "/media/short.mp4".into(),
                kind: MediaKind::Video,
                duration: Some(2.0),
                ..MediaItem::default()
            },
            MediaItem {
                id: "m3".into(),
                name: "long.mp4".into(),
                path: "/media/long.mp4".into(),
                kind: MediaKind::Video,
                duration: Some(8.0),
                ..MediaItem::default()
            },
        ];
        project.timelines = vec![Arc::new(Timeline {
            id: "TL1".into(),
            name: "Timeline 1".into(),
            video: VideoSettings::default(),
            tracks: vec![Track {
                id: "T1".into(),
                ..Track::default()
            }],
            ..Timeline::default()
        })];
        project
    }

    #[test]
    fn rejects_scripts_larger_than_the_placement_limit() {
        let row = "00:00:00 - 00:00:01 | Image 1";
        let script = std::iter::repeat_n(row, MAX_SCRIPT_ROWS + 1)
            .collect::<Vec<_>>()
            .join("\n");
        let error = parse_script(&script).unwrap_err();
        assert_eq!(error.line, MAX_SCRIPT_ROWS + 1);
        assert!(error.reason.contains("1,000 placements"));
    }

    #[test]
    fn parses_simple_and_subrip_style_ranges() {
        let rows =
            parse_script("00:00:00 - 00:00:05 | Image 1\n00:00:05.5 --> 00:00:08.000 | short.mp4")
                .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].start, rows[0].end), (0.0, 5.0));
        assert_eq!((rows[1].start, rows[1].end), (5.5, 8.0));
        assert_eq!(rows[0].label, "Image 1");
    }

    #[test]
    fn resolver_trims_long_videos_and_slows_short_ones() {
        let project = project();
        let entries =
            parse_script("00:00:00 - 00:00:03 | short.mp4\n00:00:03 - 00:00:06 | long.mp4")
                .unwrap();
        let plan = resolve(&entries, &project, AutoMotion::None).unwrap();
        assert!((plan[0].speed - (2.0 / 3.0)).abs() < 1e-9);
        assert_eq!(plan[0].duration, 3.0);
        assert_eq!(plan[1].speed, 1.0);
        assert_eq!(plan[1].duration, 3.0);
    }

    #[test]
    fn image_ordinals_follow_bin_order_and_allow_vertical_scroll() {
        let project = project();
        let entries = parse_script("00:00:00 - 00:00:04 | Image 1").unwrap();
        let plan = resolve(&entries, &project, AutoMotion::VerticalScroll).unwrap();
        assert_eq!(plan[0].media_id, "m1");
        assert_eq!(plan[0].motion, AutoMotion::VerticalScroll);
    }

    #[test]
    fn command_places_clips_at_exact_times_and_applies_motion() {
        use crate::commands::Command;
        use crate::editor::Editor;

        let mut editor = Editor::new();
        editor.project_mut().media = project().media;
        editor
            .apply(Command::PlaceTimestampScript {
                script: "00:00:00 - 00:00:03 | short.mp4\n00:00:03 - 00:00:06 | long.mp4".into(),
                track_id: "T1".into(),
                motion: AutoMotion::ZoomIn,
            })
            .unwrap();

        let timeline = editor.project().active();
        assert_eq!(timeline.clips.len(), 2);
        assert_eq!(timeline.clips[0].start, 0.0);
        assert_eq!(timeline.clips[0].duration, 3.0);
        assert!((timeline.clips[0].speed - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(timeline.clips[1].start, 3.0);
        assert_eq!(timeline.clips[1].duration, 3.0);
        assert_eq!(timeline.clips[1].speed, 1.0);
        assert_eq!(timeline.clips[0].keys.len(), 2);
        assert_eq!(
            timeline.clips[0].keys[0].property,
            crate::model::KeyProperty::Scale
        );
    }

    #[test]
    fn a_bad_later_row_does_not_partially_place_the_script() {
        use crate::commands::Command;
        use crate::editor::Editor;

        let mut editor = Editor::new();
        editor.project_mut().media = project().media;
        let result = editor.apply(Command::PlaceTimestampScript {
            script: "00:00:00 - 00:00:03 | short.mp4\n00:00:03 - 00:00:05 | absent.mp4".into(),
            track_id: "T1".into(),
            motion: AutoMotion::None,
        });
        assert!(result.is_err());
        assert!(editor.project().active().clips.is_empty());
    }

    #[test]
    fn overlapping_script_rows_are_rejected_without_partial_placement() {
        use crate::commands::Command;
        use crate::editor::Editor;

        let mut editor = Editor::new();
        editor.project_mut().media = project().media;
        let result = editor.apply(Command::PlaceTimestampScript {
            script: "00:00:00 - 00:00:03 | short.mp4\n00:00:02 - 00:00:05 | long.mp4".into(),
            track_id: "T1".into(),
            motion: AutoMotion::None,
        });
        assert!(result.is_err());
        assert!(editor.project().active().clips.is_empty());
    }

    #[test]
    fn existing_timeline_clip_blocks_the_script_without_being_changed() {
        use crate::commands::Command;
        use crate::editor::Editor;

        let mut editor = Editor::new();
        editor.project_mut().media = project().media;
        editor
            .apply(Command::AddClip {
                media_id: "m2".into(),
                track_id: "T1".into(),
                start: 1.0,
                ripple: false,
            })
            .unwrap();

        let result = editor.apply(Command::PlaceTimestampScript {
            script: "00:00:00 - 00:00:03 | portrait.png".into(),
            track_id: "T1".into(),
            motion: AutoMotion::None,
        });
        assert!(result.is_err());
        let clips = &editor.project().active().clips;
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].start, 1.0);
        assert_eq!(clips[0].media_id, "m2");
    }

    #[test]
    fn audio_only_media_cannot_be_placed_as_a_picture() {
        let mut project = project();
        project.media.push(MediaItem {
            id: "m4".into(),
            name: "voice.wav".into(),
            path: "/media/voice.wav".into(),
            kind: MediaKind::Audio,
            duration: Some(4.0),
            ..MediaItem::default()
        });
        let entries = parse_script("00:00:00 - 00:00:04 | voice.wav").unwrap();
        let error = resolve(&entries, &project, AutoMotion::None).unwrap_err();
        assert!(error.reason.contains("audio-only media"));
    }

    #[test]
    fn duplicate_file_labels_are_rejected_as_ambiguous() {
        let mut project = project();
        project.media.push(MediaItem {
            id: "m4".into(),
            name: "portrait.png".into(),
            path: "/other/portrait.png".into(),
            kind: MediaKind::Image,
            ..MediaItem::default()
        });
        let entries = parse_script("00:00:00 - 00:00:04 | portrait.png").unwrap();
        let error = resolve(&entries, &project, AutoMotion::None).unwrap_err();
        assert!(error.reason.contains("no media item matches"));
    }

    #[test]
    fn video_without_a_probed_duration_is_not_guessed() {
        let mut project = project();
        project.media[1].duration = None;
        let entries = parse_script("00:00:00 - 00:00:03 | short.mp4").unwrap();
        let error = resolve(&entries, &project, AutoMotion::None).unwrap_err();
        assert!(error.reason.contains("duration is unavailable"));
    }

    #[test]
    fn video_stretch_beyond_sixteen_times_is_rejected() {
        let mut project = project();
        project.media[1].duration = Some(0.1);
        let entries = parse_script("00:00:00 - 00:00:03 | short.mp4").unwrap();
        let error = resolve(&entries, &project, AutoMotion::None).unwrap_err();
        assert!(error.reason.contains("maximum slow-down is 16x"));
    }

    #[test]
    fn vertical_scroll_writes_overscanned_offset_keys() {
        use crate::commands::Command;
        use crate::editor::Editor;
        use crate::model::KeyProperty;

        let mut editor = Editor::new();
        editor.project_mut().media = project().media;
        editor
            .apply(Command::PlaceTimestampScript {
                script: "00:00:00 - 00:00:04 | Image 1".into(),
                track_id: "T1".into(),
                motion: AutoMotion::VerticalScroll,
            })
            .unwrap();

        let clip = &editor.project().active().clips[0];
        assert_eq!(clip.scale, 1.25);
        assert_eq!(clip.keys.len(), 2);
        assert_eq!(clip.keys[0].property, KeyProperty::OffsetY);
        assert_eq!(clip.keys[0].at, 0.0);
        assert_eq!(clip.keys[0].value, 0.10);
        assert_eq!(clip.keys[1].property, KeyProperty::OffsetY);
        assert_eq!(clip.keys[1].at, 1.0);
        assert_eq!(clip.keys[1].value, -0.10);
    }

    #[test]
    fn rejects_missing_labels_and_non_positive_spans() {
        assert!(parse_script("00:00:01 - 00:00:01 | Image 1").is_err());
        let project = project();
        let entries = parse_script("00:00:00 - 00:00:02 | missing.png").unwrap();
        assert!(resolve(&entries, &project, AutoMotion::None).is_err());
    }
}
