use super::*;

#[derive(Debug)]
pub(super) struct BufferedSpineInput {
    pub(super) id: u64,
    pub(super) prompt: SubmittedPrompt,
    pub(super) lane: SpineInputLane,
}

#[derive(Default)]
pub(super) struct SpineInputHandoff {
    pending: VecDeque<BufferedSpineInput>,
}

impl SpineInputHandoff {
    pub(super) fn buffer(&mut self, input: BufferedSpineInput) {
        self.pending.push_back(input);
    }

    pub(super) fn take_immediate(&mut self) -> Vec<BufferedSpineInput> {
        let mut immediate = Vec::new();
        let mut deferred = VecDeque::new();
        while let Some(input) = self.pending.pop_front() {
            match input.lane {
                SpineInputLane::Immediate => immediate.push(input),
                SpineInputLane::Deferred => deferred.push_back(input),
            }
        }
        self.pending = deferred;
        immediate
    }

    pub(super) fn take_deferred(&mut self) -> Option<BufferedSpineInput> {
        let index = self
            .pending
            .iter()
            .position(|input| input.lane == SpineInputLane::Deferred)?;
        self.pending.remove(index)
    }
}
