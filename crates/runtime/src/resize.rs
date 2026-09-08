//! 실제 resize 적용의 고정 크기 증거와 세션별 최신 요청 판정.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResizeToken {
    pub owner: [u8; 16],
    pub generation: u64,
    pub owner_epoch: u64,
}

impl ResizeToken {
    pub fn is_valid(self) -> bool {
        self.generation > 0 && self.owner_epoch > 0 && self.owner != [0; 16]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResizeStamp {
    pub epoch: u64,
    pub owner_epoch: u64,
    pub token: Option<ResizeToken>,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ResizeFailure {
    MissingSession,
    Backend,
    Pty,
    Dimensions,
    SizeMismatch,
    Stale,
    Conflict,
    CounterExhausted,
    Superseded,
}

impl ResizeFailure {
    pub fn retryable(self) -> bool {
        matches!(
            self,
            Self::Backend | Self::Pty | Self::Dimensions | Self::SizeMismatch
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResizeDecision {
    Apply,
    Replay(Result<ResizeStamp, ResizeFailure>),
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ResizeRecord {
    pub token: ResizeToken,
    pub target: (u16, u16),
    pub result: Result<ResizeStamp, ResizeFailure>,
    pub stamp: Option<ResizeStamp>,
}

impl ResizeRecord {
    pub fn new(
        token: ResizeToken,
        target: (u16, u16),
        result: Result<ResizeStamp, ResizeFailure>,
        stamp: Option<ResizeStamp>,
    ) -> Self {
        Self {
            token,
            target,
            result,
            stamp,
        }
    }
    pub fn change_owner(&mut self, token: ResizeToken) -> Result<(), ResizeFailure> {
        if token == self.token {
            return Ok(());
        }
        match self.classify(token, self.target) {
            ResizeDecision::Apply => {
                self.token = token;
                Ok(())
            }
            ResizeDecision::Replay(Err(reason)) => Err(reason),
            ResizeDecision::Replay(Ok(_)) => Err(ResizeFailure::Conflict),
        }
    }

    pub fn classify(&self, token: ResizeToken, target: (u16, u16)) -> ResizeDecision {
        if token.owner_epoch < self.token.owner_epoch {
            return ResizeDecision::Replay(Err(ResizeFailure::Superseded));
        }
        if token.owner != self.token.owner {
            return match self.token.owner_epoch.checked_add(1) {
                None => ResizeDecision::Replay(Err(ResizeFailure::CounterExhausted)),
                Some(next) if token.owner_epoch == next => ResizeDecision::Apply,
                Some(_) => ResizeDecision::Replay(Err(ResizeFailure::Conflict)),
            };
        }
        if token.owner_epoch != self.token.owner_epoch {
            return ResizeDecision::Replay(Err(ResizeFailure::Conflict));
        }
        if token.generation < self.token.generation {
            return ResizeDecision::Replay(Err(ResizeFailure::Stale));
        }
        if token.generation == self.token.generation {
            if target != self.target {
                return ResizeDecision::Replay(Err(ResizeFailure::Conflict));
            }
            if self.result.is_err_and(ResizeFailure::retryable) {
                return ResizeDecision::Apply;
            }
            return ResizeDecision::Replay(self.result);
        }
        ResizeDecision::Apply
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record() -> ResizeRecord {
        let token = ResizeToken {
            owner: [1; 16],
            generation: 2,
            owner_epoch: 1,
        };
        let stamp = ResizeStamp {
            epoch: 7,
            owner_epoch: 1,
            token: Some(token),
            cols: 100,
            rows: 30,
        };
        ResizeRecord::new(token, (100, 30), Ok(stamp), Some(stamp))
    }
    #[test]
    fn tracked_resize_retry는_기존_실제적용_결과를_그대로_반환한다() {
        let record = record();
        assert_eq!(
            record.classify(record.token, record.target),
            ResizeDecision::Replay(record.result)
        );
        assert_eq!(record.stamp, record.result.ok());
    }
    #[test]
    fn tracked_resize는_이전세대와_같은세대_다른크기를_거부한다() {
        let record = record();
        assert_eq!(
            record.classify(
                ResizeToken {
                    generation: 1,
                    ..record.token
                },
                record.target
            ),
            ResizeDecision::Replay(Err(ResizeFailure::Stale))
        );
        assert_eq!(
            record.classify(record.token, (101, 30)),
            ResizeDecision::Replay(Err(ResizeFailure::Conflict))
        );
        assert_eq!(
            record.classify(
                ResizeToken {
                    generation: 3,
                    ..record.token
                },
                record.target
            ),
            ResizeDecision::Apply
        );
        assert_eq!(
            record.classify(
                ResizeToken {
                    owner: [2; 16],
                    generation: 1,
                    owner_epoch: 2
                },
                record.target
            ),
            ResizeDecision::Apply
        );
    }
    #[test]
    fn tracked_resize의_부분실패는_같은_token으로_재적용할_수_있다() {
        let mut record = record();
        record.result = Err(ResizeFailure::Pty);
        assert_eq!(
            record.classify(record.token, record.target),
            ResizeDecision::Apply
        );
        record.result = Err(ResizeFailure::CounterExhausted);
        assert_eq!(
            record.classify(record.token, record.target),
            ResizeDecision::Replay(record.result)
        );
    }

    #[test]
    fn tracked_resize는_백번_owner_교체와_늦은_이전_epoch를_유계상태로_판정한다() {
        let mut record = record();
        let first = record.token;
        for owner_epoch in 2..=101 {
            let next = ResizeToken {
                owner: [owner_epoch as u8; 16],
                generation: 1,
                owner_epoch,
            };
            assert_eq!(record.classify(next, record.target), ResizeDecision::Apply);
            record.change_owner(next).unwrap();
        }
        assert_eq!(
            record.classify(first, record.target),
            ResizeDecision::Replay(Err(ResizeFailure::Superseded))
        );
        let racing = ResizeToken {
            owner: [201; 16],
            ..record.token
        };
        assert_eq!(
            record.classify(racing, record.target),
            ResizeDecision::Replay(Err(ResizeFailure::Conflict))
        );
        record.token.owner_epoch = u64::MAX;
        let wrapped = ResizeToken {
            owner: [202; 16],
            owner_epoch: 1,
            generation: 1,
        };
        assert_eq!(
            record.classify(wrapped, record.target),
            ResizeDecision::Replay(Err(ResizeFailure::Superseded))
        );
        assert_eq!(record.token.owner_epoch.checked_add(1), None);
        let exhausted = ResizeToken {
            owner_epoch: u64::MAX,
            ..wrapped
        };
        assert_eq!(
            record.classify(exhausted, record.target),
            ResizeDecision::Replay(Err(ResizeFailure::CounterExhausted))
        );
    }
}
