//! The trained mask network, stepped one frame at a time.
//!
//! Two paths meet in a head every bin shares:
//!
//! * **Context.** A dense layer, a stacked GRU and a dense layer give each bin
//!   two values from the whole spectrum's recent past.
//! * **Local shape.** A causal convolution over the last four frames and five
//!   neighbouring bins gives each bin sixteen values of its own recent shape —
//!   what tells a steady partial from a hit.
//! * **Head.** Per bin, those sixteen, the two context values and the bin's own
//!   feature go through a 16-unit layer to one sigmoid: the share of the bin
//!   that is not drums.
//!
//! The weights come from `training/export.py` (format documented there); the
//! layout and equations are PyTorch's (`training/dsil.py`,
//! `DrumSilencerNet2`), so a frame here is the same arithmetic as a frame
//! there. Everything is allocated when the weights are parsed; [`Model::step`]
//! only reads them and writes into [`ModelState`].

/// What a weight file declares about the framing it was trained with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Framing {
    pub window: usize,
    pub hop: usize,
    pub bins: usize,
    pub tau_seconds: f32,
    pub level_floor: f32,
    pub feature_scale: f32,
    pub feature_clamp: f32,
}

struct Gru {
    /// `[3H, H]`, gate rows r, z, n.
    w_ih: Box<[f32]>,
    /// `[3H, H]`.
    w_hh: Box<[f32]>,
    b_ih: Box<[f32]>,
    b_hh: Box<[f32]>,
}

pub struct Model {
    pub framing: Framing,
    hidden: usize,
    channels: usize,
    conv_time: usize,
    conv_freq: usize,
    head: usize,
    /// `[H, K]`.
    in_w: Box<[f32]>,
    in_b: Box<[f32]>,
    layers: Box<[Gru]>,
    /// `[2K, H]`.
    ctx_w: Box<[f32]>,
    ctx_b: Box<[f32]>,
    /// `[C, T, F]`, row `t = 0` the oldest frame.
    conv_w: Box<[f32]>,
    conv_b: Box<[f32]>,
    /// `[HEAD, C + 3]`.
    head1_w: Box<[f32]>,
    head1_b: Box<[f32]>,
    head2_w: Box<[f32]>,
    head2_b: f32,
}

/// One stream's recurrent state, feature history and scratch.
pub struct ModelState {
    /// The hidden state of every layer, `[layers][H]`.
    hidden: Box<[f32]>,
    /// The last `T` frames of features, a ring of `[T][K]`; zero before the
    /// stream starts, as the training pads it.
    history: Box<[f32]>,
    /// Where the newest frame is in `history`.
    newest: usize,
    x: Box<[f32]>,
    gi: Box<[f32]>,
    gh: Box<[f32]>,
    ctx: Box<[f32]>,
    /// The head's inputs as rows over every bin, `[C + 3][K]`: the
    /// convolution's channels, the two context values, the features.
    rows: Box<[f32]>,
    /// One head unit over every bin, and the head's output.
    unit: Box<[f32]>,
    out: Box<[f32]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelError {
    BadMagic,
    Version(u32),
    Truncated,
    Shape(&'static str),
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], ModelError> {
        if self.bytes.len() < n {
            return Err(ModelError::Truncated);
        }
        let (head, tail) = self.bytes.split_at(n);
        self.bytes = tail;
        Ok(head)
    }

    fn u32(&mut self) -> Result<u32, ModelError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn f32(&mut self) -> Result<f32, ModelError> {
        Ok(f32::from_bits(self.u32()?))
    }

    fn tensor(&mut self, len: usize) -> Result<Box<[f32]>, ModelError> {
        let bytes = self.take(len * 4)?;
        Ok(bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect())
    }
}

impl Model {
    pub fn parse(bytes: &[u8]) -> Result<Self, ModelError> {
        let mut r = Reader { bytes };
        if r.take(4)? != b"DSIL" {
            return Err(ModelError::BadMagic);
        }
        let version = r.u32()?;
        if version != 2 {
            return Err(ModelError::Version(version));
        }
        let window = r.u32()? as usize;
        let hop = r.u32()? as usize;
        let bins = r.u32()? as usize;
        let hidden = r.u32()? as usize;
        let layer_count = r.u32()? as usize;
        let channels = r.u32()? as usize;
        let conv_time = r.u32()? as usize;
        let conv_freq = r.u32()? as usize;
        let head = r.u32()? as usize;
        if window == 0 || hop == 0 || 2 * hop > window || bins != window / 2 + 1 {
            return Err(ModelError::Shape("framing"));
        }
        if hidden == 0 || hidden > 4_096 || layer_count == 0 || layer_count > 8 {
            return Err(ModelError::Shape("network"));
        }
        if channels == 0
            || channels > 256
            || conv_time == 0
            || conv_time > 64
            || conv_freq % 2 == 0
            || conv_freq > 63
            || head == 0
            || head > 256
        {
            return Err(ModelError::Shape("convolution"));
        }
        let framing = Framing {
            window,
            hop,
            bins,
            tau_seconds: r.f32()?,
            level_floor: r.f32()?,
            feature_scale: r.f32()?,
            feature_clamp: r.f32()?,
        };
        let in_w = r.tensor(hidden * bins)?;
        let in_b = r.tensor(hidden)?;
        let mut layers = Vec::with_capacity(layer_count);
        for _ in 0..layer_count {
            layers.push(Gru {
                w_ih: r.tensor(3 * hidden * hidden)?,
                w_hh: r.tensor(3 * hidden * hidden)?,
                b_ih: r.tensor(3 * hidden)?,
                b_hh: r.tensor(3 * hidden)?,
            });
        }
        let ctx_w = r.tensor(2 * bins * hidden)?;
        let ctx_b = r.tensor(2 * bins)?;
        let conv_w = r.tensor(channels * conv_time * conv_freq)?;
        let conv_b = r.tensor(channels)?;
        let head1_w = r.tensor(head * (channels + 3))?;
        let head1_b = r.tensor(head)?;
        let head2_w = r.tensor(head)?;
        let head2_b = r.f32()?;
        if !r.bytes.is_empty() {
            return Err(ModelError::Shape("trailing bytes"));
        }
        Ok(Self {
            framing,
            hidden,
            channels,
            conv_time,
            conv_freq,
            head,
            in_w,
            in_b,
            layers: layers.into_boxed_slice(),
            ctx_w,
            ctx_b,
            conv_w,
            conv_b,
            head1_w,
            head1_b,
            head2_w,
            head2_b,
        })
    }

    pub fn hidden(&self) -> usize {
        self.hidden
    }

    pub fn new_state(&self) -> ModelState {
        let h = self.hidden;
        let k = self.framing.bins;
        ModelState {
            hidden: vec![0.0; h * self.layers.len()].into_boxed_slice(),
            history: vec![0.0; self.conv_time * k].into_boxed_slice(),
            newest: 0,
            x: vec![0.0; h].into_boxed_slice(),
            gi: vec![0.0; 3 * h].into_boxed_slice(),
            gh: vec![0.0; 3 * h].into_boxed_slice(),
            ctx: vec![0.0; 2 * k].into_boxed_slice(),
            rows: vec![0.0; (self.channels + 3) * k].into_boxed_slice(),
            unit: vec![0.0; k].into_boxed_slice(),
            out: vec![0.0; k].into_boxed_slice(),
        }
    }

    /// One frame: `features` (K) in, the keep-mask (K, each in 0..1) out.
    pub fn step(&self, state: &mut ModelState, features: &[f32], mask: &mut [f32]) {
        let h = self.hidden;
        let k = self.framing.bins;
        debug_assert_eq!(features.len(), k);
        debug_assert_eq!(mask.len(), k);

        // Context: the whole spectrum through the GRU.
        matvec(&self.in_w, &self.in_b, features, &mut state.x);
        for v in state.x.iter_mut() {
            *v = v.max(0.0);
        }
        for (layer, gru) in self.layers.iter().enumerate() {
            let hidden = &mut state.hidden[layer * h..(layer + 1) * h];
            matvec(&gru.w_ih, &gru.b_ih, &state.x, &mut state.gi);
            matvec(&gru.w_hh, &gru.b_hh, hidden, &mut state.gh);
            for j in 0..h {
                let r = sigmoid(state.gi[j] + state.gh[j]);
                let z = sigmoid(state.gi[h + j] + state.gh[h + j]);
                let n = (state.gi[2 * h + j] + r * state.gh[2 * h + j]).tanh();
                let next = (1.0 - z) * n + z * hidden[j];
                hidden[j] = next;
                state.x[j] = next;
            }
        }
        matvec(&self.ctx_w, &self.ctx_b, &state.x, &mut state.ctx);

        // This frame joins the history, replacing the oldest.
        let t_len = self.conv_time;
        state.newest = (state.newest + 1) % t_len;
        state.history[state.newest * k..(state.newest + 1) * k].copy_from_slice(features);

        // Every per-bin step runs as a row over all bins, so it vectorises.
        let c_len = self.channels;
        let f_len = self.conv_freq;
        let reach = f_len / 2;
        let (conv_rows, rest) = state.rows.split_at_mut(c_len * k);
        // Local shape: the causal convolution, one channel at a time.
        for (c, local) in conv_rows.chunks_exact_mut(k).enumerate() {
            local.fill(self.conv_b[c]);
            for t in 0..t_len {
                let frame = (state.newest + 1 + t) % t_len;
                let row = &state.history[frame * k..(frame + 1) * k];
                let w = &self.conv_w[(c * t_len + t) * f_len..(c * t_len + t + 1) * f_len];
                for (q, &wq) in w.iter().enumerate() {
                    // out[bin] += wq * row[bin + q - reach], inside the bins.
                    if q >= reach {
                        let shift = q - reach;
                        for (o, x) in local[..k - shift].iter_mut().zip(&row[shift..]) {
                            *o += wq * x;
                        }
                    } else {
                        let shift = reach - q;
                        for (o, x) in local[shift..].iter_mut().zip(&row[..k - shift]) {
                            *o += wq * x;
                        }
                    }
                }
            }
            for v in local.iter_mut() {
                *v = v.max(0.0);
            }
        }
        let (ctx_even, rest) = rest.split_at_mut(k);
        let (ctx_odd, feat_row) = rest.split_at_mut(k);
        for bin in 0..k {
            ctx_even[bin] = state.ctx[2 * bin];
            ctx_odd[bin] = state.ctx[2 * bin + 1];
        }
        feat_row.copy_from_slice(features);

        // Head: each unit is a weighted sum of the input rows.
        let inputs = c_len + 3;
        state.out.fill(self.head2_b);
        for (j, weights) in self.head1_w.chunks_exact(inputs).enumerate() {
            state.unit.fill(self.head1_b[j]);
            for (&w, row) in weights.iter().zip(state.rows.chunks_exact(k)) {
                for (u, x) in state.unit.iter_mut().zip(row) {
                    *u += w * x;
                }
            }
            let w2 = self.head2_w[j];
            for (o, u) in state.out.iter_mut().zip(state.unit.iter()) {
                *o += w2 * u.max(0.0);
            }
        }
        for (m, &o) in mask.iter_mut().zip(state.out.iter()) {
            *m = sigmoid(o);
        }
    }
}

impl ModelState {
    pub fn reset(&mut self) {
        self.hidden.fill(0.0);
        self.history.fill(0.0);
        self.newest = 0;
    }
}

#[inline]
fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

/// `out = W x + b`, `W` row-major `[out.len(), x.len()]`. Eight running sums
/// per row so the compiler vectorises the dot product.
#[inline]
fn matvec(w: &[f32], b: &[f32], x: &[f32], out: &mut [f32]) {
    let cols = x.len();
    debug_assert_eq!(w.len(), out.len() * cols);
    for (row, (o, &bias)) in w.chunks_exact(cols).zip(out.iter_mut().zip(b)) {
        let mut acc = [0.0f32; 8];
        let mut wc = row.chunks_exact(8);
        let mut xc = x.chunks_exact(8);
        for (wv, xv) in (&mut wc).zip(&mut xc) {
            for i in 0..8 {
                acc[i] += wv[i] * xv[i];
            }
        }
        let mut sum = bias + acc.iter().sum::<f32>();
        for (wv, xv) in wc.remainder().iter().zip(xc.remainder()) {
            sum += wv * xv;
        }
        *o = sum;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matvec_matches_the_naive_product() {
        let (rows, cols) = (5, 19);
        let w: Vec<f32> = (0..rows * cols).map(|i| (i as f32 * 0.37).sin()).collect();
        let b: Vec<f32> = (0..rows).map(|i| i as f32 * 0.1).collect();
        let x: Vec<f32> = (0..cols).map(|i| (i as f32 * 0.11).cos()).collect();
        let mut out = vec![0.0; rows];
        matvec(&w, &b, &x, &mut out);
        for r in 0..rows {
            let naive: f32 = b[r] + (0..cols).map(|c| w[r * cols + c] * x[c]).sum::<f32>();
            assert!((out[r] - naive).abs() < 1.0e-5);
        }
    }

    #[test]
    fn malformed_files_are_rejected() {
        assert_eq!(Model::parse(b"NOPE").err(), Some(ModelError::BadMagic));
        assert_eq!(Model::parse(b"DSIL").err(), Some(ModelError::Truncated));
        let mut header = b"DSIL".to_vec();
        for v in [1u32, 1024, 128, 513, 8, 1] {
            header.extend(v.to_le_bytes());
        }
        assert_eq!(Model::parse(&header).err(), Some(ModelError::Version(1)));
    }
}
