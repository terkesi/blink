use super::{BATCH_SIZE, Prepared};
use crate::{
    provider::{Batch, Candidate, Failure, Judgment},
    source::Control,
};
use std::{
    collections::{BTreeSet, VecDeque},
    ops::Range,
};

const ROUTE_BYTES: usize = 48 * 1024;

struct Region {
    windows: Range<usize>,
    path: String,
    preview: String,
    start_line: usize,
    end_line: usize,
    priority: Option<f64>,
}

pub(super) struct Frontier {
    routing: bool,
    regions: Vec<Region>,
    baseline: VecDeque<usize>,
    routed: VecDeque<usize>,
    admitted: Vec<bool>,
    turn: usize,
}

impl Frontier {
    pub(super) fn new(prepared: &Prepared, routing: bool) -> Self {
        let selected: BTreeSet<_> = prepared.selected.iter().copied().collect();
        Self {
            routing,
            regions: Vec::new(),
            baseline: prepared
                .selected
                .iter()
                .copied()
                .chain(
                    (0..prepared.windows.len())
                        .filter(|index| routing && !selected.contains(index)),
                )
                .collect(),
            routed: VecDeque::new(),
            admitted: vec![false; prepared.windows.len()],
            turn: 0,
        }
    }

    pub(super) fn routes(
        &mut self,
        prepared: &Prepared,
        query: &str,
        control: &mut dyn FnMut() -> Control,
    ) -> Result<Vec<(Batch, Vec<usize>)>, Failure> {
        if !self.routing {
            return Ok(Vec::new());
        }
        if prepared.windows.len() <= BATCH_SIZE {
            let ids: Vec<_> = (0..prepared.windows.len()).collect();
            if ids.is_empty()
                || super::source_batch(prepared, query, &ids)?.encoded_len() <= 32 * 1024
            {
                return Ok(Vec::new());
            }
        }
        let mut files = VecDeque::new();
        let mut window_start = 0;
        for (file_id, file) in prepared.snapshot.files().iter().enumerate() {
            let first = self.regions.len();
            let window_end = window_start
                + prepared.windows[window_start..]
                    .iter()
                    .take_while(|window| window.file == file_id)
                    .count();
            let windows = &prepared.windows[window_start..window_end];
            let text = file.text();
            let lines = file.line_offsets();
            let mut start = 0;
            while start < text.len() {
                if control() != Control::Continue {
                    return Ok(Vec::new());
                }
                let start_line = lines.partition_point(|&offset| offset <= start);
                let mut end = lines
                    .get(start_line - 1 + 120)
                    .copied()
                    .unwrap_or(text.len())
                    .min(start.saturating_add(6144))
                    .min(text.len());
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                let end_line = lines.partition_point(|&offset| offset < end);
                let from = windows.partition_point(|window| window.end <= start) + window_start;
                let to = windows.partition_point(|window| window.start < end) + window_start;
                if from < to {
                    let mut excerpt_bytes = 64;
                    let mut region = Region {
                        windows: from..to,
                        path: short_path(file.path()),
                        preview: preview(&text[start..end], excerpt_bytes),
                        start_line,
                        end_line,
                        priority: None,
                    };
                    let name = format!("r{}", self.regions.len());
                    while Batch::route_card_len(&region.candidate(&name)) > 360 {
                        if control() != Control::Continue {
                            return Ok(Vec::new());
                        }
                        if excerpt_bytes > 0 {
                            excerpt_bytes = excerpt_bytes.saturating_sub(4);
                            region.preview = preview(&text[start..end], excerpt_bytes);
                        } else {
                            region.path = "shortened".into();
                        }
                    }
                    self.regions.push(region);
                }
                start = end;
            }
            let last = self.regions.len();
            if first < last {
                let best = (first..last)
                    .max_by_key(|&index| {
                        let rank = self.regions[index]
                            .windows
                            .clone()
                            .map(|window| prepared.windows[window].rank)
                            .max()
                            .unwrap_or(0);
                        (rank, std::cmp::Reverse(index))
                    })
                    .expect("nonempty regions");
                let mut order = VecDeque::from([best]);
                if last - 1 != best {
                    order.push_back(last - 1);
                }
                order.extend((first..last).filter(|&index| index != best && index != last - 1));
                files.push_back(order);
            }
            window_start = window_end;
        }
        let mut order = VecDeque::new();
        while let Some(mut file) = files.pop_front() {
            if control() != Control::Continue {
                return Ok(Vec::new());
            }
            order.push_back(file.pop_front().expect("nonempty file"));
            if !file.is_empty() {
                files.push_back(file);
            }
        }
        let mut batches = Vec::new();
        while !order.is_empty() && batches.len() < 2 {
            let mut ids = Vec::new();
            let mut packed = None;
            while ids.len() < 128 {
                if control() != Control::Continue {
                    return Ok(Vec::new());
                }
                let Some(id) = order.pop_front() else {
                    break;
                };
                ids.push(id);
                let names: Vec<_> = ids.iter().map(|id| format!("r{id}")).collect();
                let cards: Vec<_> = ids
                    .iter()
                    .zip(&names)
                    .map(|(&id, name)| self.regions[id].candidate(name))
                    .collect();
                let batch = Batch::encode_routes(query, &cards)?;
                if batch.encoded_len() > ROUTE_BYTES {
                    ids.pop();
                    if !ids.is_empty() {
                        order.push_front(id);
                        break;
                    }
                } else {
                    packed = Some(batch);
                }
            }
            if let Some(batch) = packed {
                batches.push((batch, ids));
            }
        }
        Ok(batches)
    }

    pub(super) fn observe(&mut self, ids: &[usize], judgments: &[Judgment]) {
        for judgment in judgments {
            let id = judgment
                .name
                .strip_prefix('r')
                .and_then(|name| name.parse::<usize>().ok())
                .expect("provider validates route names");
            debug_assert!(ids.contains(&id));
            self.regions[id].priority = judgment.probability;
        }
        let mut ranked: Vec<_> = self
            .regions
            .iter()
            .enumerate()
            .filter_map(|(id, region)| region.priority.map(|priority| (id, priority)))
            .collect();
        ranked.sort_by(|(a, pa), (b, pb)| pb.total_cmp(pa).then(a.cmp(b)));
        self.routed = ranked.into_iter().map(|(id, _)| id).collect();
    }

    pub(super) fn next(&mut self, control: &mut dyn FnMut() -> Control) -> Option<Vec<usize>> {
        let mut indices = Vec::new();
        while indices.len() < BATCH_SIZE {
            if control() != Control::Continue {
                break;
            }
            let routed = if self.turn % 4 != 3 {
                self.routed_window()
            } else {
                None
            };
            let next = routed
                .or_else(|| {
                    while let Some(index) = self.baseline.pop_front() {
                        if !self.admitted[index] {
                            return Some(index);
                        }
                    }
                    None
                })
                .or_else(|| self.routed_window());
            let Some(index) = next else {
                break;
            };
            self.admitted[index] = true;
            self.turn += 1;
            indices.push(index);
        }
        (!indices.is_empty()).then_some(indices)
    }

    fn routed_window(&mut self) -> Option<usize> {
        while let Some(id) = self.routed.pop_front() {
            let region = &mut self.regions[id];
            if let Some(index) = region.windows.find(|&index| !self.admitted[index]) {
                if !region.windows.is_empty() {
                    self.routed.push_front(id);
                }
                return Some(index);
            }
        }
        None
    }
}

impl Region {
    fn candidate<'a>(&'a self, name: &'a str) -> Candidate<'a> {
        Candidate {
            name,
            path: &self.path,
            text: &self.preview,
            start_line: self.start_line,
            end_line: self.end_line,
        }
    }
}

fn short_path(path: &str) -> String {
    if path.len() <= 80 {
        return path.into();
    }
    let leaf = path.rsplit('/').next().expect("path is nonempty");
    let mut start = leaf.len().saturating_sub(60);
    while !leaf.is_char_boundary(start) {
        start += 1;
    }
    format!("shortened/...{}", &leaf[start..])
}

fn preview(text: &str, excerpt_bytes: usize) -> String {
    let lines: Vec<_> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let declarations: Vec<_> = lines
        .iter()
        .copied()
        .filter(|line| {
            [
                "fn ",
                "def ",
                "function ",
                "class ",
                "impl ",
                "pub ",
                "const ",
                "let ",
            ]
            .iter()
            .any(|prefix| line.starts_with(prefix))
        })
        .collect();
    let anchors = if declarations.is_empty() {
        &lines
    } else {
        &declarations
    };
    if anchors.is_empty() {
        return "[empty preview]".into();
    }
    [0, anchors.len() / 2, anchors.len() - 1]
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|index| {
            let line = anchors[index];
            let mut end = line.len().min(excerpt_bytes);
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            format!("{} […]", &line[..end])
        })
        .collect::<Vec<_>>()
        .join("\n")
}
