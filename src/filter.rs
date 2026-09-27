//! **The voice filter hooks**: processor chains the game plugs in, at two stages.
//!
//! - [`FilterStage::Outgoing`] — the SPEAKER side: this player's own voice, after the mic gain and
//!   before the encoder. Runs on every captured frame while the microphone is open (a stateful
//!   filter sees a continuous signal), so the mic-test loopback sounds exactly like what the
//!   others hear. Typical use: a voice changer / scrambler that others must hear.
//! - [`FilterStage::Incoming`] — the LISTENER side: each remote speaker's decoded voice, in playout
//!   order, just before it is mixed ([`FilterContext::speaker`] says whose). Typical use: a
//!   per-speaker effect only this listener hears (distortion by distance, a radio effect).
//!
//! An empty chain is a pass-through. Two ways to plug in:
//!
//! ```
//! use bevy_voice_chat::prelude::*;
//!
//! // 1. A type implementing VoiceFilter (the game tunes it later through `get_mut`).
//! struct Robot { amount: f32 }
//! impl VoiceFilter for Robot {
//!     fn name(&self) -> &str { "robot" }
//!     fn process(&mut self, frame: &mut [f32], _ctx: &FilterContext) {
//!         for s in frame.iter_mut() { *s = (*s * (1.0 + self.amount)).clamp(-1.0, 1.0); }
//!     }
//! }
//! let mut filters = VoiceFilters::default();
//! filters.add(FilterStage::Outgoing, 100, Robot { amount: 0.5 });
//! if let Some(r) = filters.get_mut::<Robot>() { r.amount = 0.2; }
//!
//! // 2. A closure (game state can come in through an Arc the game also holds).
//! filters.add_fn(FilterStage::Incoming, 0, "half", |frame, ctx| {
//!     if ctx.speaker.is_some() { frame.iter_mut().for_each(|s| *s *= 0.5); }
//! });
//! assert_eq!(filters.names(FilterStage::Incoming), vec!["half".to_string()]);
//! ```

use bevy_ecs::prelude::Resource;
use std::any::Any;

use crate::SpeakerId;

/// Where in the pipeline a filter runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FilterStage {
    /// This player's own voice, after the mic gain, before the encoder.
    Outgoing,
    /// A remote speaker's decoded voice, before mixing.
    Incoming,
}

/// What a filter may know about the frame it processes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FilterContext {
    /// The sample rate ([`crate::codec::VOICE_RATE`]).
    pub sample_rate: u32,
    /// Which stage runs.
    pub stage: FilterStage,
    /// Incoming: whose voice; outgoing: `None` (this player).
    pub speaker: Option<SpeakerId>,
    /// Outgoing: the frame goes out to the others this time (voice activity / push-to-talk).
    pub transmitting: bool,
    /// Outgoing: the mic test — the frame comes back to this player only.
    pub loopback: bool,
}

/// One processor. Runs on the main thread, 50 times a second per stream: keep it cheap.
pub trait VoiceFilter: Any + Send + Sync {
    /// A readable name (status lines).
    fn name(&self) -> &str;
    /// Process one mono frame in place (`-1..=1`; the chain clamps and removes NaN after you).
    fn process(&mut self, frame: &mut [f32], ctx: &FilterContext);
    /// The microphone (re)opened / the stream restarted: drop any history.
    fn reset(&mut self) {}
}

/// A closure as a filter ([`VoiceFilters::add_fn`]).
pub struct FnFilter<F> {
    name: String,
    f: F,
}

impl<F: FnMut(&mut [f32], &FilterContext) + Send + Sync + 'static> VoiceFilter for FnFilter<F> {
    fn name(&self) -> &str {
        &self.name
    }

    fn process(&mut self, frame: &mut [f32], ctx: &FilterContext) {
        (self.f)(frame, ctx);
    }
}

type Chain = Vec<(i32, Box<dyn VoiceFilter>)>;

/// The two filter chains (resource). The game adds its filters here; never networked.
#[derive(Resource, Default)]
pub struct VoiceFilters {
    outgoing: Chain,
    incoming: Chain,
}

impl VoiceFilters {
    fn chain(&mut self, stage: FilterStage) -> &mut Chain {
        match stage {
            FilterStage::Outgoing => &mut self.outgoing,
            FilterStage::Incoming => &mut self.incoming,
        }
    }

    /// Add a filter to `stage`; lower `order` runs first, equal orders keep insertion order.
    pub fn add<F: VoiceFilter>(&mut self, stage: FilterStage, order: i32, filter: F) -> &mut Self {
        let chain = self.chain(stage);
        let at = chain.iter().position(|(o, _)| *o > order).unwrap_or(chain.len());
        chain.insert(at, (order, Box::new(filter)));
        self
    }

    /// Add a closure as a filter.
    pub fn add_fn(
        &mut self,
        stage: FilterStage,
        order: i32,
        name: impl Into<String>,
        f: impl FnMut(&mut [f32], &FilterContext) + Send + Sync + 'static,
    ) -> &mut Self {
        self.add(stage, order, FnFilter { name: name.into(), f })
    }

    /// The first filter of type `F` in either chain (the game tunes its own filter this way).
    pub fn get_mut<F: VoiceFilter>(&mut self) -> Option<&mut F> {
        self.outgoing.iter_mut().chain(self.incoming.iter_mut()).find_map(|(_, f)| (f.as_mut() as &mut dyn Any).downcast_mut::<F>())
    }

    /// Remove every filter of type `F`; returns how many.
    pub fn remove<F: VoiceFilter>(&mut self) -> usize {
        let before = self.outgoing.len() + self.incoming.len();
        let keep = |(_, f): &(i32, Box<dyn VoiceFilter>)| !(f.as_ref() as &dyn Any).is::<F>();
        self.outgoing.retain(keep);
        self.incoming.retain(keep);
        before - self.outgoing.len() - self.incoming.len()
    }

    /// The names of `stage`'s filters in run order (empty = pass-through).
    pub fn names(&self, stage: FilterStage) -> Vec<String> {
        let chain = match stage {
            FilterStage::Outgoing => &self.outgoing,
            FilterStage::Incoming => &self.incoming,
        };
        chain.iter().map(|(_, f)| f.name().to_string()).collect()
    }

    /// No filter in `stage`.
    pub fn is_empty(&self, stage: FilterStage) -> bool {
        self.names(stage).is_empty()
    }

    /// Run `ctx.stage`'s chain in order. After every filter the frame is sanitised (NaN -> 0,
    /// clamped to `-1..=1`): a misbehaving filter can never blow up the encoder or the ears.
    pub fn run(&mut self, frame: &mut [f32], ctx: &FilterContext) {
        for (_, f) in self.chain(ctx.stage) {
            f.process(frame, ctx);
            for s in frame.iter_mut() {
                *s = if s.is_finite() { s.clamp(-1.0, 1.0) } else { 0.0 };
            }
        }
    }

    /// Reset every filter (both stages).
    pub fn reset(&mut self) {
        for (_, f) in self.outgoing.iter_mut().chain(self.incoming.iter_mut()) {
            f.reset();
        }
    }
}

/// A trivial filter (tests, and a template): multiply by a fixed gain.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GainFilter(pub f32);

impl VoiceFilter for GainFilter {
    fn name(&self) -> &str {
        "gain"
    }

    fn process(&mut self, frame: &mut [f32], _ctx: &FilterContext) {
        for s in frame {
            *s *= self.0;
        }
    }
}
