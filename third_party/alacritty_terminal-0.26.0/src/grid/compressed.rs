//! 스크롤백으로 밀려난 행의 압축 표현 (deppy-sijo 옵션 D — 스크롤백 RSS 절감).
//!
//! alacritty는 한 행을 `Row<Cell>`(셀당 24B × 폭 + 헤더)로 저장한다. 스크롤아웃된
//! 행은 대부분 "뒤쪽 공백 + 몇 개의 속성 구간"이라, 여기서는 `text: 셀 문자 나열`,
//! `runs: (fg,bg,flags) run-length`, `extras: 희소 zerowidth/underline/hyperlink`로
//! 압축한다. `Cell`의 공개 API만 써서 왕복 무손실이며, alacritty 타입(`CellExtra`)의
//! 프라이버시를 건드리지 않는다.
//!
//! 이 파일은 순수 데이터 구조 + codec이다. Storage 링버퍼에 이를 앉히는(곁가지
//! 배열 + compress_line/read_line) 것은 `storage.rs`이며, 트리거(누가 언제 압축을
//! 호출하는가)는 아직 없다 — 후속 PR에서 연결한다.

#![allow(dead_code)]

use crate::index::Column;
use crate::term::cell::{Cell, Flags, Hyperlink};
use crate::vte::ansi::Color;

use super::row::Row;

/// 연속된 셀이 공유하는 색/플래그 구간.
#[derive(Clone, Debug)]
struct AttrRun {
    len: u16,
    fg: Color,
    bg: Color,
    flags: Flags,
}

/// 드물게만 존재하는 셀 부가 정보(alacritty의 `CellExtra`에 대응).
#[derive(Clone, Debug)]
struct ExtraData {
    zerowidth: Box<[char]>,
    underline_color: Option<Color>,
    hyperlink: Option<Hyperlink>,
}

/// 압축된 한 행. content_len(마지막 non-default 셀 + 1)까지만 저장하고, 그 뒤는
/// 복원 시 `Cell::default()`로 채운다(뒤쪽 공백 압축의 핵심).
#[derive(Clone, Debug)]
pub(crate) struct CompressedRow {
    /// content 범위 각 셀의 문자. 길이(char 수) == content_len.
    text: Box<str>,
    /// content 범위의 (fg,bg,flags) run-length. len 합 == content_len.
    runs: Box<[AttrRun]>,
    /// extra를 가진 셀만 (열 인덱스, 데이터).
    extras: Box<[(u16, ExtraData)]>,
    /// 원본 Row의 occ(점유 셀 수) — 왕복 보존.
    occ: u16,
}

impl CompressedRow {
    /// 이 압축 행이 실제로 점유하는 힙 바이트 추정 — RSS 예산/실측용.
    pub(crate) fn heap_bytes(&self) -> usize {
        let extras_cells: usize = self
            .extras
            .iter()
            .map(|(_, e)| e.zerowidth.len() * std::mem::size_of::<char>())
            .sum();
        self.text.len()
            + self.runs.len() * std::mem::size_of::<AttrRun>()
            + self.extras.len() * std::mem::size_of::<(u16, ExtraData)>()
            + extras_cells
    }

    /// `Row<Cell>`을 압축한다. 뒤쪽 default 셀은 저장하지 않는다.
    pub(crate) fn encode(row: &Row<Cell>, columns: usize) -> CompressedRow {
        let default = Cell::default();
        let width = columns.min(row.len());
        // content_len: 마지막으로 default가 아닌 셀 인덱스 + 1.
        let mut content_len = 0usize;
        for col in (0..width).rev() {
            if row[Column(col)] != default {
                content_len = col + 1;
                break;
            }
        }

        let mut text = String::with_capacity(content_len);
        let mut runs: Vec<AttrRun> = Vec::new();
        let mut extras: Vec<(u16, ExtraData)> = Vec::new();
        for col in 0..content_len {
            let cell = &row[Column(col)];
            text.push(cell.c);
            match runs.last_mut() {
                Some(run) if run.fg == cell.fg && run.bg == cell.bg && run.flags == cell.flags => {
                    run.len += 1;
                }
                _ => runs.push(AttrRun {
                    len: 1,
                    fg: cell.fg,
                    bg: cell.bg,
                    flags: cell.flags,
                }),
            }
            if cell.extra.is_some() {
                extras.push((
                    col as u16,
                    ExtraData {
                        zerowidth: cell
                            .zerowidth()
                            .map(|z| z.to_vec().into_boxed_slice())
                            .unwrap_or_default(),
                        underline_color: cell.underline_color(),
                        hyperlink: cell.hyperlink(),
                    },
                ));
            }
        }

        CompressedRow {
            text: text.into_boxed_str(),
            runs: runs.into_boxed_slice(),
            extras: extras.into_boxed_slice(),
            occ: row.occ.min(u16::MAX as usize) as u16,
        }
    }

    /// 압축 행을 `columns` 폭의 `Row<Cell>`로 복원한다.
    pub(crate) fn decode(&self, columns: usize) -> Row<Cell> {
        let mut cells: Vec<Cell> = (0..columns).map(|_| Cell::default()).collect();
        // run-length로 열별 속성을 펼치며 문자와 함께 채운다.
        let mut chars = self.text.chars();
        let mut col = 0usize;
        for run in self.runs.iter() {
            for _ in 0..run.len {
                if col >= columns {
                    break;
                }
                let Some(c) = chars.next() else { break };
                let cell = &mut cells[col];
                cell.c = c;
                cell.fg = run.fg;
                cell.bg = run.bg;
                cell.flags = run.flags;
                col += 1;
            }
        }
        // 희소 extra 복원 (공개 setter만 사용).
        for (ecol, data) in self.extras.iter() {
            let idx = *ecol as usize;
            if idx >= columns {
                continue;
            }
            let cell = &mut cells[idx];
            for &ch in data.zerowidth.iter() {
                cell.push_zerowidth(ch);
            }
            if data.underline_color.is_some() {
                cell.set_underline_color(data.underline_color);
            }
            if data.hyperlink.is_some() {
                cell.set_hyperlink(data.hyperlink.clone());
            }
        }
        Row::from_vec(cells, self.occ as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vte::ansi::{NamedColor, Rgb};

    fn columns() -> usize {
        20
    }

    /// 셀 벡터로 Row를 만들고 압축→복원이 원본과 완전히 같은지 검증.
    fn roundtrip(cells: Vec<Cell>) {
        let cols = columns();
        assert_eq!(cells.len(), cols, "테스트는 폭에 맞춘 벡터를 준다");
        let occ = cols;
        let row = Row::from_vec(cells.clone(), occ);
        let compressed = CompressedRow::encode(&row, cols);
        let restored = compressed.decode(cols);
        for col in 0..cols {
            assert_eq!(
                row[Column(col)],
                restored[Column(col)],
                "col {col} 왕복 불일치"
            );
        }
    }

    fn blank_row() -> Vec<Cell> {
        (0..columns()).map(|_| Cell::default()).collect()
    }

    #[test]
    fn 빈_행_왕복() {
        roundtrip(blank_row());
    }

    #[test]
    fn 평문_ascii_뒤공백_왕복() {
        let mut cells = blank_row();
        for (i, ch) in "hello".chars().enumerate() {
            cells[i].c = ch;
        }
        roundtrip(cells);
    }

    #[test]
    fn 여러_색구간_왕복() {
        let mut cells = blank_row();
        for i in 0..columns() {
            cells[i].c = 'x';
            cells[i].fg = if i < 5 {
                Color::Named(NamedColor::Red)
            } else if i < 10 {
                Color::Spec(Rgb { r: 1, g: 2, b: 3 })
            } else {
                Color::Indexed(42)
            };
            if i % 2 == 0 {
                cells[i].flags.insert(Flags::BOLD);
            }
        }
        roundtrip(cells);
    }

    #[test]
    fn cjk_wide_char_왕복() {
        let mut cells = blank_row();
        cells[0].c = '한';
        cells[0].flags.insert(Flags::WIDE_CHAR);
        cells[1].c = ' ';
        cells[1].flags.insert(Flags::WIDE_CHAR_SPACER);
        cells[2].c = '글';
        cells[2].flags.insert(Flags::WIDE_CHAR);
        cells[3].c = ' ';
        cells[3].flags.insert(Flags::WIDE_CHAR_SPACER);
        roundtrip(cells);
    }

    #[test]
    fn zerowidth_underline_hyperlink_왕복() {
        let mut cells = blank_row();
        cells[0].c = 'e';
        cells[0].push_zerowidth('\u{0301}'); // combining accent
        cells[1].c = 'u';
        cells[1].set_underline_color(Some(Color::Named(NamedColor::Green)));
        cells[2].c = 'h';
        cells[2].set_hyperlink(Some(Hyperlink::new(
            Some("id1"),
            "https://example.com".to_string(),
        )));
        roundtrip(cells);
    }

    #[test]
    fn wrapline_플래그_왕복() {
        let mut cells = blank_row();
        for i in 0..columns() {
            cells[i].c = 'a';
        }
        // WRAPLINE은 보통 마지막 셀에 걸린다.
        cells[columns() - 1].flags.insert(Flags::WRAPLINE);
        roundtrip(cells);
    }

    #[test]
    fn 압축률이_원셀_배열보다_작다() {
        // 일반 로그류 라인(80자 상당을 20폭으로 축소): 텍스트 + 소수 run.
        let mut cells = blank_row();
        for i in 0..12 {
            cells[i].c = 'x';
        }
        let row = Row::from_vec(cells, columns());
        let compressed = CompressedRow::encode(&row, columns());
        let raw = columns() * std::mem::size_of::<Cell>();
        assert!(
            compressed.heap_bytes() < raw,
            "compressed {} >= raw {}",
            compressed.heap_bytes(),
            raw
        );
    }
}
