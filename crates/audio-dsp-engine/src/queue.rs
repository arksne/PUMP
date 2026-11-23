//! Очередь с историей и повторами; детерминированный порядок по близости BPM и энергии.
use crate::{Error, Result, range};
use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RepeatMode {
    #[default]
    Off,
    One,
    All,
}
#[derive(Clone, Debug)]
pub struct PlaybackQueue<T> {
    upcoming: VecDeque<T>,
    current: Option<T>,
    history: VecDeque<T>,
    history_limit: usize,
    repeat: RepeatMode,
}
impl<T: Clone> Default for PlaybackQueue<T> {
    fn default() -> Self {
        Self::new(100)
    }
}
impl<T: Clone> PlaybackQueue<T> {
    pub fn new(history_limit: usize) -> Self {
        Self {
            upcoming: VecDeque::new(),
            current: None,
            history: VecDeque::new(),
            history_limit,
            repeat: RepeatMode::Off,
        }
    }
    pub fn enqueue(&mut self, track: T) {
        self.upcoming.push_back(track);
    }
    pub fn enqueue_next(&mut self, track: T) {
        self.upcoming.push_front(track);
    }
    pub fn current(&self) -> Option<&T> {
        self.current.as_ref()
    }
    pub fn upcoming(&self) -> &VecDeque<T> {
        &self.upcoming
    }
    pub fn history(&self) -> &VecDeque<T> {
        &self.history
    }
    pub fn set_repeat(&mut self, mode: RepeatMode) {
        self.repeat = mode;
    }
    pub fn repeat(&self) -> RepeatMode {
        self.repeat
    }
    pub fn remove(&mut self, index: usize) -> Result<T> {
        self.upcoming
            .remove(index)
            .ok_or(Error::Invalid("queue index"))
    }
    pub fn move_track(&mut self, from: usize, to: usize) -> Result<()> {
        if from >= self.upcoming.len() || to >= self.upcoming.len() {
            return Err(Error::Invalid("queue index"));
        }
        let track = self.upcoming.remove(from).expect("validated index");
        self.upcoming.insert(to, track);
        Ok(())
    }
    fn remember(&mut self, track: T) {
        if self.history_limit > 0 {
            if self.history.len() == self.history_limit {
                self.history.pop_front();
            }
            self.history.push_back(track);
        }
    }
    /// Явное переключение: Repeat One игнорируется, Repeat All возвращает трек в конец очереди.
    pub fn skip_next(&mut self) -> Option<&T> {
        if let Some(track) = self.current.take() {
            if self.repeat == RepeatMode::All {
                self.upcoming.push_back(track.clone());
            }
            self.remember(track);
        }
        self.current = self.upcoming.pop_front();
        self.current()
    }
    pub fn on_track_end(&mut self) -> Option<&T> {
        if self.repeat == RepeatMode::One && self.current.is_some() {
            self.current()
        } else {
            self.skip_next()
        }
    }
    /// Возвращает предыдущий трек; текущий становится первым ожидающим.
    pub fn previous(&mut self) -> Option<&T> {
        if let Some(track) = self.history.pop_back() {
            if let Some(current) = self.current.replace(track) {
                self.upcoming.push_front(current);
            }
        }
        self.current()
    }
    pub fn clear(&mut self) {
        self.upcoming.clear();
        self.current = None;
        self.history.clear();
    }
    /// Применяет перестановку только к ожидающим трекам. Проверка атомарна.
    pub fn reorder(&mut self, indices: &[usize]) -> Result<()> {
        if indices.len() != self.upcoming.len() {
            return Err(Error::Invalid("queue permutation length"));
        }
        let mut seen = vec![false; indices.len()];
        for &i in indices {
            if i >= seen.len() || seen[i] {
                return Err(Error::Invalid("queue permutation"));
            }
            seen[i] = true;
        }
        self.upcoming = indices.iter().map(|&i| self.upcoming[i].clone()).collect();
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TrackFeatures {
    pub bpm: Option<f64>,
    pub energy: f64,
}
impl TrackFeatures {
    pub fn validate(&self) -> Result<()> {
        if let Some(bpm) = self.bpm {
            range(bpm, 20.0, 400.0, "BPM (20..400)")?;
        }
        range(self.energy, 0.0, 1.0, "track energy (0..1)")
    }
}
#[derive(Clone, Copy, Debug)]
pub struct OrderingWeights {
    pub tempo: f64,
    pub energy: f64,
    pub allow_half_double: bool,
}
impl Default for OrderingWeights {
    fn default() -> Self {
        Self {
            tempo: 0.7,
            energy: 0.3,
            allow_half_double: true,
        }
    }
}
/// Жадный ближайший сосед, O(n²); равные стоимости сохраняют исходный порядок.
/// BPM отсутствует: фиксированная стоимость 1 по темпу. Без seed первый трек остаётся первым.
pub fn order_by_bpm_energy(
    tracks: &[TrackFeatures],
    seed: Option<TrackFeatures>,
    weights: OrderingWeights,
) -> Result<Vec<usize>> {
    range(weights.tempo, 0.0, 1.0, "tempo weight")?;
    range(weights.energy, 0.0, 1.0, "energy weight")?;
    if weights.tempo + weights.energy == 0.0 {
        return Err(Error::Invalid("nonzero ordering weights"));
    }
    for t in tracks {
        t.validate()?;
    }
    if let Some(t) = seed {
        t.validate()?;
    }
    let mut used = vec![false; tracks.len()];
    let mut output = Vec::with_capacity(tracks.len());
    let mut last = seed;
    while output.len() < tracks.len() {
        let mut best = None;
        for (i, &t) in tracks.iter().enumerate() {
            if used[i] {
                continue;
            }
            let cost = if let Some(previous) = last {
                let tempo = match (previous.bpm, t.bpm) {
                    (Some(a), Some(b)) => {
                        let d = (a / b).log2().abs();
                        if weights.allow_half_double {
                            d.min((d - 1.0).abs())
                        } else {
                            d
                        }
                    }
                    _ => 1.0,
                };
                weights.tempo * tempo + weights.energy * (previous.energy - t.energy).abs()
            } else {
                0.0
            };
            if best.is_none_or(|(_, c)| cost < c) {
                best = Some((i, cost));
            }
        }
        let i = best.expect("at least one unused track").0;
        used[i] = true;
        output.push(i);
        last = Some(tracks[i]);
    }
    Ok(output)
}
