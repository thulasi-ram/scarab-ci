//! The automata and the streaming core of the log redactor (ADR-0082 §3,
//! "Union of matches").
//!
//! For the bytes `S` of one read of a part, the marked intervals are every
//! occurrence of every value, the leading interval (the longest start of
//! `S` of at least `min_suffix` bytes that ends a value), and at a
//! confirmed end the whole-part interval (all of `S`, when it lies inside a
//! value with at least one more byte of it after `S` and is `min_suffix`
//! bytes or longer) and the trailing interval (the shortest end of `S`
//! that begins a value). Intervals that share a byte form one span; each
//! span is stored as one `***`; touching spans stay separate.
//!
//! [`Trie`] (Aho–Corasick) finds the occurrences and where a value could
//! still start; [`Sam`] (a suffix automaton of the values) decides the
//! leading interval. Both are flat arrays (no node owns a heap block) and
//! are built without recursion; `logs::ValueSet` builds them within the
//! process budget and keeps them while a reader holds them.
//!
//! The model this was ported from matched a brute-force reference; the
//! tests in `logs.rs` compare this code with the same reference.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::hash::{BuildHasher, Hasher};
use std::sync::{Arc, OnceLock};

/// What a redacted span is replaced with.
pub(crate) const REDACTED: &[u8] = b"***";

const NONE: u32 = u32::MAX;

// ---------------------------------------------------------------------------
// Aho–Corasick trie
// ---------------------------------------------------------------------------

/// A trie of the values with failure links. Node 0 is the root.
pub(crate) struct Trie {
    /// The edges of node `n` are `start[n]..start[n + 1]` in `edge_byte`
    /// (sorted) and `edge_to`.
    start: Vec<u32>,
    edge_byte: Vec<u8>,
    edge_to: Vec<u32>,
    fail: Vec<u32>,
    /// The length of the longest value that ends the node's string (0 if
    /// none).
    best: Vec<u32>,
    /// The length of the longest suffix of the node's string that is a
    /// proper prefix of some value: the depth of the first node on its
    /// failure chain, itself included, that has a child.
    pend: Vec<u32>,
}

impl Trie {
    /// Build the trie of the non-empty `values`. Sorted insertion: the
    /// children of a node are made in byte order, so a counting sort by
    /// parent gives each node its edges sorted. Breadth-first for the
    /// failure links; no recursion.
    pub(crate) fn build(values: &[Vec<u8>]) -> Self {
        let mut sorted: Vec<&[u8]> = values
            .iter()
            .map(Vec::as_slice)
            .filter(|v| !v.is_empty())
            .collect();
        sorted.sort_unstable();
        sorted.dedup();
        let mut depth: Vec<u32> = vec![0];
        let mut best: Vec<u32> = vec![0];
        // (parent, byte, child), each parent's children in byte order.
        let mut edges: Vec<(u32, u8, u32)> = Vec::new();
        let mut path: Vec<u32> = vec![0];
        let mut prev: &[u8] = &[];
        for v in sorted {
            let common = prev.iter().zip(v).take_while(|(a, b)| a == b).count();
            path.truncate(common + 1);
            for (d, &b) in v.iter().enumerate().skip(common) {
                let child = depth.len() as u32;
                depth.push(d as u32 + 1);
                best.push(0);
                edges.push((path[d], b, child));
                path.push(child);
            }
            best[path[v.len()] as usize] = v.len() as u32;
            prev = v;
        }
        let n = depth.len();
        let mut start = vec![0u32; n + 1];
        for &(p, _, _) in &edges {
            start[p as usize + 1] += 1;
        }
        for i in 0..n {
            start[i + 1] += start[i];
        }
        let mut edge_byte = vec![0u8; edges.len()];
        let mut edge_to = vec![0u32; edges.len()];
        {
            let mut fill: Vec<u32> = start[..n].to_vec();
            for &(p, b, c) in &edges {
                let i = fill[p as usize] as usize;
                edge_byte[i] = b;
                edge_to[i] = c;
                fill[p as usize] += 1;
            }
        }
        drop(edges);
        // Grown by pushes: up to twice its length until shrunk (4 bytes per
        // node of the 21 the size bound allows for).
        best.shrink_to_fit();
        let mut trie = Trie {
            start,
            edge_byte,
            edge_to,
            fail: vec![0; n],
            best,
            pend: vec![0; n],
        };
        // Breadth first: a node's failure target is shallower, so its
        // `best` and `pend` are final before the node's are set.
        let mut order: Vec<u32> = Vec::with_capacity(n);
        order.push(0);
        let mut q = 0;
        while q < order.len() {
            let node = order[q];
            q += 1;
            let (lo, hi) = trie.edges(node);
            for i in lo..hi {
                let (b, child) = (trie.edge_byte[i], trie.edge_to[i]);
                let fail = if node == 0 {
                    0
                } else {
                    let mut f = trie.fail[node as usize];
                    loop {
                        if let Some(t) = trie.goto(f, b) {
                            break t;
                        }
                        if f == 0 {
                            break 0;
                        }
                        f = trie.fail[f as usize];
                    }
                };
                let c = child as usize;
                trie.fail[c] = fail;
                if trie.best[c] == 0 {
                    trie.best[c] = trie.best[fail as usize];
                }
                let (clo, chi) = trie.edges(child);
                trie.pend[c] = if chi > clo {
                    depth[c]
                } else {
                    trie.pend[fail as usize]
                };
                order.push(child);
            }
        }
        trie
    }

    fn edges(&self, n: u32) -> (usize, usize) {
        (
            self.start[n as usize] as usize,
            self.start[n as usize + 1] as usize,
        )
    }

    fn goto(&self, n: u32, b: u8) -> Option<u32> {
        let (lo, hi) = self.edges(n);
        self.edge_byte[lo..hi]
            .binary_search(&b)
            .ok()
            .map(|i| self.edge_to[lo + i])
    }

    /// The node after reading `b` at `n`; `steps` counts each edge tried.
    fn step(&self, mut n: u32, b: u8, steps: &mut u64) -> u32 {
        loop {
            *steps += 1;
            if let Some(t) = self.goto(n, b) {
                return t;
            }
            if n == 0 {
                return 0;
            }
            n = self.fail[n as usize];
        }
    }

    #[cfg(test)]
    pub(crate) fn nodes(&self) -> usize {
        self.fail.len()
    }

    /// The heap bytes the built trie keeps.
    #[cfg(test)]
    pub(crate) fn heap_bytes(&self) -> usize {
        self.start.capacity() * 4
            + self.edge_byte.capacity()
            + self.edge_to.capacity() * 4
            + self.fail.capacity() * 4
            + self.best.capacity() * 4
            + self.pend.capacity() * 4
    }
}

// ---------------------------------------------------------------------------
// Suffix automaton
// ---------------------------------------------------------------------------

/// A suffix automaton of all the values. Its only use is the leading
/// interval: it is read from the root and never along suffix links.
pub(crate) struct Sam {
    /// The edges of state `q` are `start[q]..start[q + 1]` in `edge_byte`
    /// (sorted) and `edge_to`.
    start: Vec<u32>,
    edge_byte: Vec<u8>,
    edge_to: Vec<u32>,
    /// The low 31 bits are `room` (the longest path out of the state: how
    /// far its strings extend inside some value); the top bit is `suffix`
    /// (the state's strings end some value).
    room: Vec<u32>,
}

const SUFFIX_BIT: u32 = 1 << 31;

/// The hash key of this process: a value's author cannot choose bytes
/// whose `(state, byte)` keys collide in the build's map.
fn process_seed() -> u64 {
    static SEED: OnceLock<u64> = OnceLock::new();
    *SEED.get_or_init(|| std::collections::hash_map::RandomState::new().hash_one(0u64))
}

/// Builds [`KeyHasher`]s keyed with [`process_seed`].
#[derive(Clone, Copy)]
struct KeyBuild(u64);

impl BuildHasher for KeyBuild {
    type Hasher = KeyHasher;
    fn build_hasher(&self) -> KeyHasher {
        KeyHasher(self.0)
    }
}

/// A hasher for `(state, byte)` keys: the key xor the process seed, then
/// the splitmix64 finaliser.
struct KeyHasher(u64);

impl Hasher for KeyHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(u64::from(b));
        }
    }
    fn write_u64(&mut self, n: u64) {
        let mut z = (n ^ self.0).wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        self.0 = z ^ (z >> 31);
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

/// The automaton while it is built: transitions as one edge array with a
/// list per state (for cloning a state) and one map from `(state, byte)`
/// to the edge (for lookups).
struct SamBuilder {
    len: Vec<u32>,
    link: Vec<u32>,
    head: Vec<u32>,
    e_byte: Vec<u8>,
    e_to: Vec<u32>,
    e_next: Vec<u32>,
    map: HashMap<u64, u32, KeyBuild>,
}

fn key(p: u32, c: u8) -> u64 {
    (u64::from(p) << 8) | u64::from(c)
}

impl SamBuilder {
    fn get(&self, p: u32, c: u8) -> Option<u32> {
        self.map.get(&key(p, c)).map(|&e| self.e_to[e as usize])
    }

    fn set(&mut self, p: u32, c: u8, to: u32) {
        match self.map.entry(key(p, c)) {
            Entry::Occupied(e) => self.e_to[*e.get() as usize] = to,
            Entry::Vacant(v) => {
                let e = self.e_to.len() as u32;
                self.e_byte.push(c);
                self.e_to.push(to);
                self.e_next.push(self.head[p as usize]);
                self.head[p as usize] = e;
                v.insert(e);
            }
        }
    }

    fn new_state(&mut self, len: u32, link: u32) -> u32 {
        self.len.push(len);
        self.link.push(link);
        self.head.push(NONE);
        (self.len.len() - 1) as u32
    }

    /// A copy of `q` with length `len`, which becomes `q`'s suffix link.
    fn clone_state(&mut self, q: u32, len: u32) -> u32 {
        let cl = self.new_state(len, self.link[q as usize]);
        let mut e = self.head[q as usize];
        while e != NONE {
            let (b, to, next) = (
                self.e_byte[e as usize],
                self.e_to[e as usize],
                self.e_next[e as usize],
            );
            self.set(cl, b, to);
            e = next;
        }
        self.link[q as usize] = cl;
        cl
    }

    /// Generalised online construction. `last` already having a
    /// transition on `c` (a value that is a prefix of one added before)
    /// takes the first branch.
    fn extend(&mut self, last: u32, c: u8) -> u32 {
        let last_len = self.len[last as usize];
        if let Some(q) = self.get(last, c) {
            if self.len[q as usize] == last_len + 1 {
                return q;
            }
            let cl = self.clone_state(q, last_len + 1);
            let mut p = last;
            while p != NONE && self.get(p, c) == Some(q) {
                self.set(p, c, cl);
                p = self.link[p as usize];
            }
            return cl;
        }
        let cur = self.new_state(last_len + 1, 0);
        let mut p = last;
        while p != NONE && self.get(p, c).is_none() {
            self.set(p, c, cur);
            p = self.link[p as usize];
        }
        if p != NONE {
            let q = self.get(p, c).expect("the loop stopped on a transition");
            if self.len[p as usize] + 1 == self.len[q as usize] {
                self.link[cur as usize] = q;
            } else {
                let cl = self.clone_state(q, self.len[p as usize] + 1);
                while p != NONE && self.get(p, c) == Some(q) {
                    self.set(p, c, cl);
                    p = self.link[p as usize];
                }
                self.link[cur as usize] = cl;
            }
        }
        cur
    }
}

impl Sam {
    /// Build the suffix automaton of the non-empty `values`.
    pub(crate) fn build(values: &[Vec<u8>]) -> Self {
        let total: usize = values.iter().map(Vec::len).sum();
        let mut b = SamBuilder {
            len: Vec::with_capacity(total + 1),
            link: Vec::with_capacity(total + 1),
            head: Vec::with_capacity(total + 1),
            e_byte: Vec::with_capacity(total),
            e_to: Vec::with_capacity(total),
            e_next: Vec::with_capacity(total),
            map: HashMap::with_capacity_and_hasher(total, KeyBuild(process_seed())),
        };
        b.new_state(0, NONE);
        for v in values.iter().filter(|v| !v.is_empty()) {
            let mut last = 0;
            for &c in v {
                last = b.extend(last, c);
            }
        }
        let states = b.len.len();
        // `suffix`, after the whole build: a clone made by a later value
        // can sit on an earlier value's suffix chain. Walk each value and
        // mark its chain; a marked state's chain is already marked.
        let mut room = vec![0u32; states];
        for v in values.iter().filter(|v| !v.is_empty()) {
            let mut q = 0u32;
            for &c in v {
                q = b.get(q, c).expect("every value is a path from the root");
            }
            while q != 0 && q != NONE && room[q as usize] & SUFFIX_BIT == 0 {
                room[q as usize] |= SUFFIX_BIT;
                q = b.link[q as usize];
            }
        }
        // `room`: a transition always goes to a longer state, so states
        // in decreasing length see their targets done. Counting sort by
        // length; no recursion.
        let max_len = b.len.iter().copied().max().unwrap_or(0) as usize;
        let mut by_len = vec![0u32; max_len + 2];
        for &l in &b.len {
            by_len[l as usize + 1] += 1;
        }
        for i in 0..=max_len {
            by_len[i + 1] += by_len[i];
        }
        let mut order = vec![0u32; states];
        for (q, &l) in b.len.iter().enumerate() {
            order[by_len[l as usize] as usize] = q as u32;
            by_len[l as usize] += 1;
        }
        drop(by_len);
        for &q in order.iter().rev() {
            let mut r = 0u32;
            let mut e = b.head[q as usize];
            while e != NONE {
                let to = b.e_to[e as usize] as usize;
                r = r.max(1 + (room[to] & !SUFFIX_BIT));
                e = b.e_next[e as usize];
            }
            room[q as usize] |= r;
        }
        drop(order);
        // Freeze into sorted flat arrays; the map is not needed for it.
        drop(std::mem::replace(
            &mut b.map,
            HashMap::with_hasher(KeyBuild(0)),
        ));
        let edges = b.e_to.len();
        let mut start = Vec::with_capacity(states + 1);
        let mut edge_byte = Vec::with_capacity(edges);
        let mut edge_to = Vec::with_capacity(edges);
        let mut tmp: Vec<(u8, u32)> = Vec::new();
        start.push(0u32);
        for q in 0..states {
            tmp.clear();
            let mut e = b.head[q];
            while e != NONE {
                tmp.push((b.e_byte[e as usize], b.e_to[e as usize]));
                e = b.e_next[e as usize];
            }
            tmp.sort_unstable_by_key(|&(c, _)| c);
            for &(c, to) in &tmp {
                edge_byte.push(c);
                edge_to.push(to);
            }
            start.push(edge_byte.len() as u32);
        }
        Sam {
            start,
            edge_byte,
            edge_to,
            room,
        }
    }

    fn next(&self, q: u32, b: u8) -> Option<u32> {
        let (lo, hi) = (
            self.start[q as usize] as usize,
            self.start[q as usize + 1] as usize,
        );
        self.edge_byte[lo..hi]
            .binary_search(&b)
            .ok()
            .map(|i| self.edge_to[lo + i])
    }

    fn suffix(&self, q: u32) -> bool {
        self.room[q as usize] & SUFFIX_BIT != 0
    }

    fn room(&self, q: u32) -> u32 {
        self.room[q as usize] & !SUFFIX_BIT
    }

    #[cfg(test)]
    pub(crate) fn states(&self) -> usize {
        self.room.len()
    }

    #[cfg(test)]
    pub(crate) fn transitions(&self) -> usize {
        self.edge_to.len()
    }

    /// The heap bytes the built automaton keeps.
    #[cfg(test)]
    pub(crate) fn heap_bytes(&self) -> usize {
        self.start.capacity() * 4
            + self.edge_byte.capacity()
            + self.edge_to.capacity() * 4
            + self.room.capacity() * 4
    }
}

// ---------------------------------------------------------------------------
// The streaming core
// ---------------------------------------------------------------------------

/// One read of a source, or one whole text. Holds the bytes from `from`
/// to `k` and the spans known inside them; emits each byte or `***` once
/// no interval found later can change it.
pub(crate) struct Core {
    trie: Arc<Trie>,
    /// `None` for a whole text (no leading interval).
    sam: Option<Arc<Sam>>,
    min_suffix: usize,
    node: u32,
    /// Bytes read.
    k: u64,
    /// While the leading interval is pending: the suffix automaton state
    /// of `S[..k]` and the best `n` so far.
    lead: Option<(u32, u64)>,
    /// The offset of `held[0]`.
    from: u64,
    held: VecDeque<u8>,
    /// Known spans in `[from, k)`: sorted, no two sharing a byte.
    spans: VecDeque<(u64, u64)>,
    /// The end of a span whose `***` was emitted and that may still grow.
    open: Option<u64>,
    /// Automaton steps taken (trie edges tried and suffix automaton steps).
    steps: u64,
}

impl Core {
    /// A core for a stream: leading interval on.
    pub(crate) fn stream(trie: Arc<Trie>, sam: Arc<Sam>, min_suffix: usize) -> Self {
        assert!(min_suffix >= 1, "the minimum leading suffix is at least 1");
        let mut c = Self::text(trie);
        c.sam = Some(sam);
        c.lead = Some((0, 0));
        c.min_suffix = min_suffix;
        c
    }

    /// A core for a whole text: occurrences only.
    pub(crate) fn text(trie: Arc<Trie>) -> Self {
        Self {
            trie,
            sam: None,
            min_suffix: usize::MAX,
            node: 0,
            k: 0,
            lead: None,
            from: 0,
            held: VecDeque::new(),
            spans: VecDeque::new(),
            open: None,
            steps: 0,
        }
    }

    fn byte(&mut self, b: u8) {
        self.node = self.trie.step(self.node, b, &mut self.steps);
        self.k += 1;
        self.held.push_back(b);
        if let Some((q, n)) = self.lead {
            let sam = self.sam.as_ref().expect("a pending lead has its automaton");
            self.steps += 1;
            let next = sam.next(q, b);
            let mut n = n;
            if let Some(q2) = next {
                if sam.suffix(q2) && self.k >= self.min_suffix as u64 {
                    n = self.k;
                }
            }
            let need = (self.min_suffix as u64).saturating_sub(self.k).max(1);
            match next {
                Some(q2) if u64::from(sam.room(q2)) >= need => self.lead = Some((q2, n)),
                _ => {
                    self.lead = None;
                    if n > 0 {
                        self.add_front(n);
                    }
                }
            }
        }
        let m = self.trie.best[self.node as usize];
        if m > 0 {
            self.add_back(self.k - u64::from(m), self.k);
        }
    }

    /// `[s, e)` extends the emitted open span when it starts before its
    /// end: the open span grows, absorbs every span that starts before its
    /// new end (which may grow it further), and the held bytes up to that
    /// end are dropped. Whether it did.
    fn extend_open(&mut self, s: u64, e: u64) -> bool {
        let Some(e0) = self.open else {
            return false;
        };
        if s >= e0 {
            return false;
        }
        let mut end = e0.max(e);
        while let Some(&(a, c)) = self.spans.front() {
            if a >= end {
                break;
            }
            end = end.max(c);
            self.spans.pop_front();
        }
        self.drop_to(end);
        self.open = Some(end);
        true
    }

    /// Add `[s, e)` that ends at or after every known span (an occurrence
    /// or the trailing interval): merge from the back, as far as it
    /// reaches.
    fn add_back(&mut self, s: u64, e: u64) {
        if self.extend_open(s, e) {
            return;
        }
        debug_assert!(
            s >= self.from,
            "an interval never starts before the held bytes"
        );
        let (mut s, mut e) = (s, e);
        while let Some(&(a, c)) = self.spans.back() {
            if c <= s {
                break;
            }
            s = s.min(a);
            e = e.max(c);
            self.spans.pop_back();
        }
        self.spans.push_back((s, e));
    }

    /// Add `[0, e)` (the leading or the whole-part interval; nothing has
    /// been emitted yet): merge from the front, as far as it reaches.
    fn add_front(&mut self, e: u64) {
        if self.extend_open(0, e) {
            return;
        }
        let mut e = e;
        while let Some(&(a, c)) = self.spans.front() {
            if a >= e {
                break;
            }
            e = e.max(c);
            self.spans.pop_front();
        }
        self.spans.push_front((0, e));
    }

    fn drop_to(&mut self, pos: u64) {
        if pos > self.from {
            self.held.drain(..(pos - self.from) as usize);
            self.from = pos;
        }
    }

    fn take_to(&mut self, pos: u64, out: &mut Vec<u8>) {
        if pos > self.from {
            out.extend(self.held.drain(..(pos - self.from) as usize));
            self.from = pos;
        }
    }

    /// Emit what no later interval can change: every interval found later
    /// starts at `h` or after.
    fn emit(&mut self, h: u64) -> Vec<u8> {
        let mut out = Vec::new();
        if let Some(e0) = self.open {
            if e0 > h {
                return out;
            }
            self.open = None;
        }
        loop {
            let next = self.spans.front().copied();
            let stop = next.map_or(h, |(a, _)| a.min(h));
            self.take_to(stop, &mut out);
            let Some((a, c)) = next else { break };
            if a > h {
                break;
            }
            self.spans.pop_front();
            out.extend_from_slice(REDACTED);
            if c > h {
                self.drop_to(c);
                self.open = Some(c);
                break;
            }
            self.drop_to(c);
        }
        out
    }

    /// Read `src`; returns what can be emitted now.
    pub(crate) fn push(&mut self, src: &[u8]) -> Vec<u8> {
        for &b in src {
            self.byte(b);
        }
        if self.lead.is_some() {
            return Vec::new();
        }
        let h = self.k - u64::from(self.trie.pend[self.node as usize]);
        self.emit(h)
    }

    /// No more bytes. `confirmed`: the end of a source (the whole-part and
    /// trailing intervals apply); else a whole text.
    pub(crate) fn finish(mut self, confirmed: bool) -> Vec<u8> {
        if let Some((_, n)) = self.lead.take() {
            // Still pending: `S` lies inside a value with at least one more
            // byte of it after `S`. The whole-part interval covers `[0, n)`.
            if confirmed && self.k >= self.min_suffix as u64 {
                self.add_front(self.k);
            } else if n > 0 {
                self.add_front(n);
            }
        }
        if confirmed {
            let p = u64::from(self.trie.pend[self.node as usize]);
            if p > 0 {
                self.add_back(self.k - p, self.k);
            }
        }
        let k = self.k;
        self.emit(k)
    }

    /// Bytes held now.
    #[cfg(test)]
    pub(crate) fn held_len(&self) -> usize {
        self.held.len()
    }

    #[cfg(test)]
    pub(crate) fn steps(&self) -> u64 {
        self.steps
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn substrings_of(values: &[Vec<u8>]) -> Vec<Vec<u8>> {
        let mut out = vec![Vec::new()];
        for v in values {
            for i in 0..v.len() {
                for j in i + 1..=v.len() {
                    out.push(v[i..j].to_vec());
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// The trie node of `s` (a prefix of some value).
    fn trie_node(t: &Trie, s: &[u8]) -> u32 {
        let mut n = 0;
        for &b in s {
            n = t.goto(n, b).expect("a prefix of a value");
        }
        n
    }

    fn sam_state(m: &Sam, s: &[u8]) -> Option<u32> {
        let mut q = 0;
        for &b in s {
            q = m.next(q, b)?;
        }
        Some(q)
    }

    fn values() -> impl Strategy<Value = Vec<Vec<u8>>> {
        let alphabet = prop_oneof![Just(&b"a"[..]), Just(&b"ab"[..]), Just(&b"abc"[..])];
        alphabet.prop_flat_map(|a| {
            proptest::collection::vec(
                proptest::collection::vec(proptest::sample::select(a), 1..=9),
                1..=4,
            )
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

        /// Every trie node's `best` and `pend` equal brute force over its
        /// string. Kills: `best` not inherited along the failure link;
        /// `pend` from the node only.
        #[test]
        fn trie_fields_match_brute_force(values in values()) {
            let t = Trie::build(&values);
            let mut prefixes: Vec<Vec<u8>> = values
                .iter()
                .flat_map(|v| (0..=v.len()).map(move |i| v[..i].to_vec()))
                .collect();
            prefixes.sort();
            prefixes.dedup();
            prop_assert_eq!(t.nodes(), prefixes.len());
            for p in &prefixes {
                let n = trie_node(&t, p) as usize;
                let best = (1..=p.len())
                    .filter(|&m| values.iter().any(|v| v.as_slice() == &p[p.len() - m..]))
                    .max()
                    .unwrap_or(0);
                let pend = (1..=p.len())
                    .filter(|&m| {
                        let s = &p[p.len() - m..];
                        values.iter().any(|v| v.len() > m && v.starts_with(s))
                    })
                    .max()
                    .unwrap_or(0);
                prop_assert_eq!(t.best[n] as usize, best, "best of {:?}", p);
                prop_assert_eq!(t.pend[n] as usize, pend, "pend of {:?}", p);
            }
        }

        /// Every substring of a value reaches a state from the root, no
        /// other string does, and the state's `suffix` and `room` equal
        /// brute force over that string. Kills: `suffix` marked during the
        /// build; the existing-transition branch removed.
        #[test]
        fn suffix_automaton_fields_match_brute_force(values in values()) {
            let m = Sam::build(&values);
            let total: usize = values.iter().map(Vec::len).sum();
            prop_assert!(m.states() <= 2 * total + 1);
            prop_assert!(m.transitions() <= 3 * total);
            for s in substrings_of(&values).iter().skip(1) {
                let q = sam_state(&m, s).expect("a substring is a path");
                let suffix = values.iter().any(|v| v.ends_with(s));
                let room = values
                    .iter()
                    .filter(|v| v.len() >= s.len())
                    .flat_map(|v| {
                        (0..=v.len().saturating_sub(s.len()))
                            .filter(move |&i| &v[i..i + s.len()] == s.as_slice())
                            .map(move |i| v.len() - i - s.len())
                    })
                    .max()
                    .unwrap_or(0);
                prop_assert_eq!(m.suffix(q), suffix, "suffix of {:?}", s);
                prop_assert_eq!(m.room(q) as usize, room, "room of {:?}", s);
            }
            // A string that is no substring has no state.
            let mut outside = values[0].clone();
            outside.push(b'z');
            prop_assert!(sam_state(&m, &outside).is_none());
        }
    }

    /// The size of the built automata per value byte, for the shapes with
    /// the most states (`abb…bc`) and the most transitions, and for random
    /// bytes: states at most 2 per value byte, transitions at most 3, and
    /// the kept heap at most 52 bytes per value byte (the bound the cap in
    /// `logs.rs` is chosen with). Values of 256 KiB (the cap), one random
    /// and one a 256 Ki-deep chain, build with no recursion on a test
    /// thread's stack. Kills: per-node heap blocks;
    /// a recursive `room`.
    #[test]
    fn the_built_automata_stay_under_52_bytes_per_value_byte() {
        let n = 256 * 1024;
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut random = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 24) as u8
        };
        let mut abc = vec![b'a'];
        abc.extend(std::iter::repeat_n(b'b', n - 2));
        abc.push(b'c');
        let shapes: Vec<(&str, Vec<Vec<u8>>)> = vec![
            ("random bytes", vec![(0..n).map(|_| random()).collect()]),
            ("one byte repeated", vec![vec![b'a'; n / 4]]),
            ("abb..bc", vec![abc]),
            (
                "many short tokens",
                (0..n / 160)
                    .map(|_| (0..40).map(|_| b'A' + random() % 64).collect())
                    .collect(),
            ),
        ];
        for (name, values) in shapes {
            let total: usize = values.iter().map(Vec::len).sum();
            let t = Trie::build(&values);
            let m = Sam::build(&values);
            assert!(t.nodes() <= total + 1, "{name}: trie nodes");
            assert!(m.states() <= 2 * total, "{name}: {} states", m.states());
            assert!(
                m.transitions() <= 3 * total,
                "{name}: {} transitions",
                m.transitions()
            );
            let bytes = t.heap_bytes() + m.heap_bytes();
            assert!(
                bytes <= 52 * total + 64,
                "{name}: {bytes} heap bytes for {total} value bytes"
            );
        }
    }

    /// The cost on a value of one repeated byte: at most two trie steps
    /// per source byte, and suffix automaton steps only while the leading
    /// interval is pending (fewer than the longest value). Counted, not
    /// timed. Kills: a scan that compares a value per position.
    #[test]
    fn a_repeated_byte_costs_at_most_two_steps_per_source_byte() {
        for first in *b"ab" {
            // `a…a`, and the same with `b…a` beside it (whose tail starts
            // the part).
            let mut other = vec![b'a'; 64 * 1024];
            other[0] = first;
            let values = vec![vec![b'a'; 64 * 1024], other];
            let trie = Arc::new(Trie::build(&values));
            let sam = Arc::new(Sam::build(&values));
            let mut core = Core::stream(trie, sam, 8);
            let src = vec![b'a'; 256 * 1024];
            let mut out = Vec::new();
            for c in src.chunks(7) {
                out.extend(core.push(c));
                assert!(core.held_len() < 64 * 1024);
            }
            let steps = core.steps();
            out.extend(core.finish(true));
            assert_eq!(out, b"***", "first byte {first}");
            // Two trie steps per source byte at most, plus one suffix
            // automaton step per byte while the lead is pending.
            assert!(
                steps <= 2 * src.len() as u64 + 64 * 1024,
                "first byte {first}: {steps} steps"
            );
        }
    }
}
