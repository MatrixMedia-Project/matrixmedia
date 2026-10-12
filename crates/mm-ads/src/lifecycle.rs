//! Creative status rules (spec 2026-10-11 §5.4). Media state and the admin's pause combine into
//! the one `status` the decision engine reads. Pure: `conversion_db` applies the result.

use crate::creative::AdStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Upload,
    JobSucceeded,
    JobFailed,
    Pause,
    Resume,
    Delete,
}

/// State of the creative's newest job, which decides where Resume lands without media.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LastJob {
    None,
    Open,
    Failed,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    To(AdStatus),
    Stay,
    /// 409: the event is not allowed in this state (anything but delete on a deleted creative).
    Reject,
}

pub fn next(status: AdStatus, event: Event, has_active_media: bool, last: LastJob) -> Transition {
    use AdStatus::*;
    use Event::*;
    match (status, event) {
        (Deleted, Delete | JobSucceeded | JobFailed) => Transition::Stay,
        (Deleted, _) => Transition::Reject,
        (_, Delete) => Transition::To(Deleted),
        (Paused, Pause) => Transition::Stay,
        (_, Pause) => Transition::To(Paused),
        (Paused, Resume) => Transition::To(if has_active_media {
            Ready
        } else {
            match last {
                LastJob::Open => Processing,
                LastJob::Failed => Failed,
                LastJob::None | LastJob::Done => Draft,
            }
        }),
        (_, Resume) => Transition::Stay,
        // A paused creative converts in the background and stays paused (spec §5.4).
        (Paused, Upload | JobSucceeded | JobFailed) => Transition::Stay,
        // D7: re-uploading over a live ad never takes it out of rotation.
        (Ready, Upload | JobSucceeded | JobFailed) => Transition::Stay,
        (Draft | Processing | Failed, Upload) => Transition::To(Processing),
        (Draft | Processing | Failed, JobSucceeded) => Transition::To(Ready),
        (Failed, JobFailed) => Transition::Stay,
        (Draft | Processing, JobFailed) => Transition::To(Failed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use AdStatus::*;
    use Event::*;

    /// Every state × event (feedback: enumerate state × mode × event before done). The media
    /// flag and last job only matter for Resume; that row is covered separately below.
    #[test]
    fn the_whole_matrix() {
        use Transition::{Reject as R, Stay as S, To};
        let rows: [(AdStatus, [Transition; 6]); 6] = [
            //            Upload          Succeeded      Failed         Pause        Resume  Delete
            (Draft,      [To(Processing), To(Ready),     To(Failed),    To(Paused),  S,      To(Deleted)]),
            (Processing, [To(Processing), To(Ready),     To(Failed),    To(Paused),  S,      To(Deleted)]),
            (Ready,      [S,              S,             S,             To(Paused),  S,      To(Deleted)]),
            (Failed,     [To(Processing), To(Ready),     S,             To(Paused),  S,      To(Deleted)]),
            (Paused,     [S,              S,             S,             S,           To(Draft), To(Deleted)]),
            (Deleted,    [R,              S,             S,             R,           R,      S]),
        ];
        let events = [Upload, JobSucceeded, JobFailed, Pause, Resume, Delete];
        for (status, want) in rows {
            for (e, w) in events.iter().zip(want) {
                assert_eq!(next(status, *e, false, LastJob::None), w, "{status:?} × {e:?}");
            }
        }
    }

    #[test]
    fn resume_lands_on_what_the_media_allows() {
        assert_eq!(next(Paused, Resume, true, LastJob::Failed), Transition::To(Ready));
        assert_eq!(next(Paused, Resume, false, LastJob::Open), Transition::To(Processing));
        assert_eq!(next(Paused, Resume, false, LastJob::Failed), Transition::To(Failed));
        assert_eq!(next(Paused, Resume, false, LastJob::Done), Transition::To(Draft));
    }

    #[test]
    fn status_strings_round_trip() {
        for s in [Draft, Processing, Ready, Failed, Paused, Deleted] {
            assert_eq!(AdStatus::parse(s.as_str()), Some(s));
        }
        assert_eq!(AdStatus::parse("encoding"), None);
    }
}
