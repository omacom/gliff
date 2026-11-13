//! What the client has been sent, and which parts of the latest frame
//! differ from it.

use crate::encode::Rect;

const TILE: usize = 16;

/// The latest captured frame next to the copy the client has, with the
/// tiles where they differ.
pub struct Screen {
    width: usize,
    height: usize,
    latest: Vec<u8>,
    sent: Vec<u8>,
    cols: usize,
    rows: usize,
    dirty: Vec<bool>,
}

impl Screen {
    /// A screen the client has not been sent anything of yet.
    pub fn new(width: usize, height: usize) -> Self {
        let cols = width.div_ceil(TILE);
        let rows = height.div_ceil(TILE);
        Self {
            width,
            height,
            latest: vec![0; width * height * 4],
            sent: vec![0; width * height * 4],
            cols,
            rows,
            dirty: vec![true; cols * rows],
        }
    }

    pub fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    pub fn latest(&self) -> &[u8] {
        &self.latest
    }

    /// Take a new frame of the same size. `damage` lists where it may
    /// differ from the previous one, in pixels; `None` means anywhere. Only
    /// tiles that really changed since they were sent become dirty.
    pub fn update(&mut self, pixels: &[u8], damage: Option<&[Rect]>) {
        assert_eq!(pixels.len(), self.latest.len(), "frame size changed");
        self.latest.copy_from_slice(pixels);
        let mut check = vec![damage.is_none(); self.cols * self.rows];
        for r in damage.unwrap_or_default() {
            let (x0, y0) = (usize::from(r.x) / TILE, usize::from(r.y) / TILE);
            let x1 = (usize::from(r.x) + usize::from(r.width))
                .div_ceil(TILE)
                .min(self.cols);
            let y1 = (usize::from(r.y) + usize::from(r.height))
                .div_ceil(TILE)
                .min(self.rows);
            for ty in y0..y1 {
                for tx in x0..x1 {
                    check[ty * self.cols + tx] = true;
                }
            }
        }
        for (i, checked) in check.into_iter().enumerate() {
            if checked && !self.dirty[i] {
                self.dirty[i] = self.tile_differs(i % self.cols, i / self.cols);
            }
        }
    }

    /// Make everything in `area` dirty: the client asked for all of it.
    pub fn invalidate(&mut self, area: Rect) {
        for (tx, ty) in self.tiles_in(area) {
            self.dirty[ty * self.cols + tx] = true;
        }
    }

    pub fn has_changes_in(&self, area: Rect) -> bool {
        self.tiles_in(area)
            .any(|(tx, ty)| self.dirty[ty * self.cols + tx])
    }

    /// The dirty parts of `area` as rectangles (runs of dirty tiles in each
    /// row of tiles), marked as sent.
    pub fn take_changes(&mut self, area: Rect) -> Vec<Rect> {
        let mut out = Vec::new();
        let (x0, y0, x1, y1) = self.clip(area);
        if x0 >= x1 || y0 >= y1 {
            return out;
        }
        let (tx0, ty0) = (x0 / TILE, y0 / TILE);
        let (tx1, ty1) = (x1.div_ceil(TILE), y1.div_ceil(TILE));
        for ty in ty0..ty1 {
            let mut tx = tx0;
            while tx < tx1 {
                if !self.dirty[ty * self.cols + tx] {
                    tx += 1;
                    continue;
                }
                let start = tx;
                while tx < tx1 && self.dirty[ty * self.cols + tx] {
                    tx += 1;
                }
                // Clip the run of tiles to the requested area.
                let rx0 = (start * TILE).max(x0);
                let rx1 = (tx * TILE).min(x1);
                let ry0 = (ty * TILE).max(y0);
                let ry1 = ((ty + 1) * TILE).min(y1);
                let rect = Rect {
                    x: rx0 as u16,
                    y: ry0 as u16,
                    width: (rx1 - rx0) as u16,
                    height: (ry1 - ry0) as u16,
                };
                self.mark_sent(rect);
                // A tile cut by the area's edge keeps the rest dirty.
                for t in start..tx {
                    let whole = t * TILE >= x0
                        && ((t + 1) * TILE).min(self.width) <= x1
                        && ty * TILE >= y0
                        && ((ty + 1) * TILE).min(self.height) <= y1;
                    if whole {
                        self.dirty[ty * self.cols + t] = false;
                    } else {
                        self.dirty[ty * self.cols + t] = self.tile_differs(t, ty);
                    }
                }
                out.push(rect);
            }
        }
        out
    }

    fn clip(&self, a: Rect) -> (usize, usize, usize, usize) {
        let x0 = usize::from(a.x).min(self.width);
        let y0 = usize::from(a.y).min(self.height);
        let x1 = (usize::from(a.x) + usize::from(a.width)).min(self.width);
        let y1 = (usize::from(a.y) + usize::from(a.height)).min(self.height);
        (x0, y0, x1, y1)
    }

    fn tiles_in(&self, area: Rect) -> impl Iterator<Item = (usize, usize)> {
        let (x0, y0, x1, y1) = self.clip(area);
        let (tx0, ty0) = (x0 / TILE, y0 / TILE);
        let (tx1, ty1) = if x0 < x1 && y0 < y1 {
            (x1.div_ceil(TILE), y1.div_ceil(TILE))
        } else {
            (tx0, ty0)
        };
        (ty0..ty1).flat_map(move |ty| (tx0..tx1).map(move |tx| (tx, ty)))
    }

    fn rows_of(
        &self,
        x0: usize,
        x1: usize,
        y0: usize,
        y1: usize,
    ) -> impl Iterator<Item = std::ops::Range<usize>> + '_ {
        (y0..y1).map(move |y| (y * self.width + x0) * 4..(y * self.width + x1) * 4)
    }

    fn tile_differs(&self, tx: usize, ty: usize) -> bool {
        let x1 = ((tx + 1) * TILE).min(self.width);
        let y1 = ((ty + 1) * TILE).min(self.height);
        self.rows_of(tx * TILE, x1, ty * TILE, y1)
            .any(|r| self.latest[r.clone()] != self.sent[r])
    }

    fn mark_sent(&mut self, r: Rect) {
        let (x0, y0) = (usize::from(r.x), usize::from(r.y));
        let (x1, y1) = (x0 + usize::from(r.width), y0 + usize::from(r.height));
        let ranges: Vec<_> = self.rows_of(x0, x1, y0, y1).collect();
        for range in ranges {
            let (latest, sent) = (&self.latest[range.clone()], &mut self.sent[range]);
            sent.copy_from_slice(latest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full(w: u16, h: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        }
    }

    fn frame(w: usize, h: usize, v: u8) -> Vec<u8> {
        vec![v; w * h * 4]
    }

    #[test]
    fn the_first_request_gets_everything_then_nothing() {
        let mut s = Screen::new(50, 20);
        s.update(&frame(50, 20, 1), None);
        let rects = s.take_changes(full(50, 20));
        // Two rows of tiles, each a single run across the width.
        assert_eq!(rects.len(), 2);
        assert_eq!(
            rects
                .iter()
                .map(|r| u32::from(r.width) * u32::from(r.height))
                .sum::<u32>(),
            1000
        );
        assert!(!s.has_changes_in(full(50, 20)));
        s.update(&frame(50, 20, 1), None);
        assert!(s.take_changes(full(50, 20)).is_empty());
    }

    #[test]
    fn only_the_changed_tiles_are_sent() {
        let mut s = Screen::new(64, 64);
        s.update(&frame(64, 64, 0), None);
        s.take_changes(full(64, 64));
        let mut next = frame(64, 64, 0);
        // One pixel at 40,20 changes: tile (2, 1).
        let i = (20 * 64 + 40) * 4;
        next[i] = 9;
        let damage = [Rect {
            x: 40,
            y: 20,
            width: 1,
            height: 1,
        }];
        s.update(&next, Some(&damage));
        assert_eq!(
            s.take_changes(full(64, 64)),
            vec![Rect {
                x: 32,
                y: 16,
                width: 16,
                height: 16
            }]
        );
    }

    #[test]
    fn damage_that_changed_nothing_sends_nothing() {
        let mut s = Screen::new(32, 32);
        s.update(&frame(32, 32, 5), None);
        s.take_changes(full(32, 32));
        s.update(&frame(32, 32, 5), Some(&[full(32, 32)]));
        assert!(!s.has_changes_in(full(32, 32)));
    }

    #[test]
    fn a_partial_request_leaves_the_rest_dirty() {
        let mut s = Screen::new(32, 16);
        s.update(&frame(32, 16, 3), None);
        // Half of the first tile only.
        let got = s.take_changes(Rect {
            x: 0,
            y: 0,
            width: 8,
            height: 16,
        });
        assert_eq!(
            got,
            vec![Rect {
                x: 0,
                y: 0,
                width: 8,
                height: 16
            }]
        );
        // That tile's other half and the second tile are still owed.
        assert_eq!(
            s.take_changes(full(32, 16)),
            vec![Rect {
                x: 0,
                y: 0,
                width: 32,
                height: 16
            }]
        );
        assert!(!s.has_changes_in(full(32, 16)));
    }

    #[test]
    fn invalidate_resends_what_the_client_already_has() {
        let mut s = Screen::new(16, 16);
        s.update(&frame(16, 16, 1), None);
        s.take_changes(full(16, 16));
        s.invalidate(full(16, 16));
        assert_eq!(s.take_changes(full(16, 16)).len(), 1);
    }
}
