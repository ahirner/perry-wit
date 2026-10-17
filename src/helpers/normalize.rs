//! Allocation-free Unicode normalization (`NFC`, `NFD`, `NFKC`, `NFKD`).

pub(super) fn normalize_utf8(input: &str, out_ptr: u32, compose: bool, compat: bool) -> u64 {
    let mut sink = OutSink {
        out_ptr,
        byte_len: 0,
        scalar_len: 0,
        overflow: false,
    };
    let mut run = CharRun::<64>::new();
    for ch in input.chars() {
        decompose_into(ch, compat, &mut run, &mut sink, compose);
        if sink.overflow {
            return u64::MAX;
        }
    }
    flush_run(&mut run, &mut sink, compose);
    if sink.overflow {
        u64::MAX
    } else {
        u64::from(sink.byte_len) | (u64::from(sink.scalar_len) << 32)
    }
}

struct OutSink {
    out_ptr: u32,
    byte_len: u32,
    scalar_len: u32,
    overflow: bool,
}

impl OutSink {
    fn emit(&mut self, ch: char) {
        if self.overflow {
            return;
        }
        let width = ch.len_utf8() as u32;
        let Some(next_len) = self.byte_len.checked_add(width) else {
            self.overflow = true;
            return;
        };
        if self.out_ptr != 0 {
            let Some(dest) = self.out_ptr.checked_add(self.byte_len) else {
                self.overflow = true;
                return;
            };
            // SAFETY: measurement reserved the complete output, including this scalar.
            let output =
                unsafe { core::slice::from_raw_parts_mut(dest as *mut u8, width as usize) };
            ch.encode_utf8(output);
        }
        self.byte_len = next_len;
        self.scalar_len += 1;
    }
}

struct CharRun<const N: usize> {
    buf: [char; N],
    len: usize,
}

impl<const N: usize> CharRun<N> {
    const fn new() -> Self {
        Self {
            buf: ['\0'; N],
            len: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn is_full(&self) -> bool {
        self.len == N
    }

    fn push(&mut self, ch: char) {
        if self.len < N {
            self.buf[self.len] = ch;
            self.len += 1;
        }
    }

    fn pop(&mut self) -> Option<char> {
        if self.len == 0 {
            None
        } else {
            self.len -= 1;
            Some(self.buf[self.len])
        }
    }
}

fn decompose_into(ch: char, compat: bool, run: &mut CharRun<64>, out: &mut OutSink, compose: bool) {
    let mut stack = CharRun::<8>::new();
    stack.push(ch);
    while let Some(curr) = stack.pop() {
        let code = curr as u32;
        if (0xAC00..=0xD7A3).contains(&code) {
            let s_index = code - 0xAC00;
            let l = char::from_u32(0x1100 + s_index / 588).unwrap_or(curr);
            let v = char::from_u32(0x1161 + (s_index % 588) / 28).unwrap_or(curr);
            push_scalar(l, run, out, compose);
            push_scalar(v, run, out, compose);
            let t = s_index % 28;
            if t > 0
                && let Some(tc) = char::from_u32(0x11A7 + t)
            {
                push_scalar(tc, run, out, compose);
            }
            continue;
        }
        if let Some((a, b)) = canonical_pair(curr) {
            stack.push(b);
            stack.push(a);
            continue;
        }
        if compat {
            if (0xFF01..=0xFF5E).contains(&code)
                && let Some(ascii) = char::from_u32(code - 0xFEE0)
            {
                push_scalar(ascii, run, out, compose);
                continue;
            }
            if let Some((a, b, c)) = compat_triple(curr) {
                if let Some(c) = c {
                    stack.push(c);
                }
                if let Some(b) = b {
                    stack.push(b);
                }
                stack.push(a);
                continue;
            }
        }
        push_scalar(curr, run, out, compose);
    }
}

fn push_scalar(ch: char, run: &mut CharRun<64>, out: &mut OutSink, compose: bool) {
    let ccc = combining_class(ch);
    if ccc == 0 && !is_hangul_jamo_medial_or_final(ch) && !run.is_empty() {
        flush_run(run, out, compose);
    }
    if run.is_full() {
        flush_run(run, out, compose);
    }
    run.push(ch);
}

fn flush_run(run: &mut CharRun<64>, out: &mut OutSink, compose: bool) {
    if run.is_empty() {
        return;
    }
    let len = run.len;
    for i in 1..len {
        let mut j = i;
        while j > 0 {
            let ccc_prev = combining_class(run.buf[j - 1]);
            let ccc_curr = combining_class(run.buf[j]);
            if ccc_prev > 0 && ccc_curr > 0 && ccc_prev > ccc_curr {
                run.buf.swap(j - 1, j);
                j -= 1;
            } else {
                break;
            }
        }
    }
    if compose && run.len > 1 {
        let mut starter_idx = 0usize;
        let mut last_ccc = combining_class(run.buf[0]);
        let mut write = 1usize;
        for read in 1..run.len {
            let ch = run.buf[read];
            let ch_ccc = combining_class(ch);
            let can_compose =
                (last_ccc < ch_ccc || last_ccc == 0) && combining_class(run.buf[starter_idx]) == 0;
            if can_compose && let Some(composed) = compose_pair(run.buf[starter_idx], ch) {
                run.buf[starter_idx] = composed;
                continue;
            }
            if ch_ccc == 0 {
                starter_idx = write;
            }
            last_ccc = ch_ccc;
            run.buf[write] = ch;
            write += 1;
        }
        run.len = write;
    }
    for i in 0..run.len {
        out.emit(run.buf[i]);
    }
    run.len = 0;
}

fn is_hangul_jamo_medial_or_final(ch: char) -> bool {
    matches!(ch as u32, 0x1161..=0x1175 | 0x11A8..=0x11C2)
}

fn combining_class(ch: char) -> u8 {
    match ch as u32 {
        0x0334..=0x0338 => 1,
        0x0321..=0x0322 | 0x0327..=0x0328 => 202,
        0x031B => 216,
        0x0316..=0x0319 | 0x031C..=0x0320 | 0x0323..=0x0326 | 0x0329..=0x0333 | 0x0339..=0x033C => {
            220
        }
        0x0300..=0x0314 | 0x033D..=0x0344 | 0x0346 | 0x034A..=0x034C => 230,
        0x0315 | 0x031A => 232,
        0x0345 => 240,
        _ => 0,
    }
}

fn compat_triple(ch: char) -> Option<(char, Option<char>, Option<char>)> {
    Some(match ch {
        '\u{00A0}' => (' ', None, None),
        '\u{00A8}' => (' ', Some('\u{0308}'), None),
        '\u{00AA}' => ('a', None, None),
        '\u{00AF}' => (' ', Some('\u{0304}'), None),
        '\u{00B2}' => ('2', None, None),
        '\u{00B3}' => ('3', None, None),
        '\u{00B4}' => (' ', Some('\u{0301}'), None),
        '\u{00B5}' => ('\u{03BC}', None, None),
        '\u{00B8}' => (' ', Some('\u{0327}'), None),
        '\u{00B9}' => ('1', None, None),
        '\u{00BA}' => ('o', None, None),
        '\u{00BC}' => ('1', Some('\u{2044}'), Some('4')),
        '\u{00BD}' => ('1', Some('\u{2044}'), Some('2')),
        '\u{00BE}' => ('3', Some('\u{2044}'), Some('4')),
        '\u{0132}' => ('I', Some('J'), None),
        '\u{0133}' => ('i', Some('j'), None),
        '\u{013F}' => ('L', Some('\u{00B7}'), None),
        '\u{0140}' => ('l', Some('\u{00B7}'), None),
        '\u{0149}' => ('\u{02BC}', Some('n'), None),
        '\u{017F}' => ('s', None, None),
        '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => (' ', None, None),
        '\u{FB00}' => ('f', Some('f'), None),
        '\u{FB01}' => ('f', Some('i'), None),
        '\u{FB02}' => ('f', Some('l'), None),
        '\u{FB03}' => ('f', Some('f'), Some('i')),
        '\u{FB04}' => ('f', Some('f'), Some('l')),
        _ => return None,
    })
}

fn canonical_pair(ch: char) -> Option<(char, char)> {
    Some(match ch {
        '\u{00C0}' => ('A', '\u{0300}'),
        '\u{00C1}' => ('A', '\u{0301}'),
        '\u{00C2}' => ('A', '\u{0302}'),
        '\u{00C3}' => ('A', '\u{0303}'),
        '\u{00C4}' => ('A', '\u{0308}'),
        '\u{00C5}' | '\u{212B}' => ('A', '\u{030A}'),
        '\u{00C7}' => ('C', '\u{0327}'),
        '\u{00C8}' => ('E', '\u{0300}'),
        '\u{00C9}' => ('E', '\u{0301}'),
        '\u{00CA}' => ('E', '\u{0302}'),
        '\u{00CB}' => ('E', '\u{0308}'),
        '\u{00CC}' => ('I', '\u{0300}'),
        '\u{00CD}' => ('I', '\u{0301}'),
        '\u{00CE}' => ('I', '\u{0302}'),
        '\u{00CF}' => ('I', '\u{0308}'),
        '\u{00D1}' => ('N', '\u{0303}'),
        '\u{00D2}' => ('O', '\u{0300}'),
        '\u{00D3}' => ('O', '\u{0301}'),
        '\u{00D4}' => ('O', '\u{0302}'),
        '\u{00D5}' => ('O', '\u{0303}'),
        '\u{00D6}' => ('O', '\u{0308}'),
        '\u{00D9}' => ('U', '\u{0300}'),
        '\u{00DA}' => ('U', '\u{0301}'),
        '\u{00DB}' => ('U', '\u{0302}'),
        '\u{00DC}' => ('U', '\u{0308}'),
        '\u{00DD}' => ('Y', '\u{0301}'),
        '\u{00E0}' => ('a', '\u{0300}'),
        '\u{00E1}' => ('a', '\u{0301}'),
        '\u{00E2}' => ('a', '\u{0302}'),
        '\u{00E3}' => ('a', '\u{0303}'),
        '\u{00E4}' => ('a', '\u{0308}'),
        '\u{00E5}' => ('a', '\u{030A}'),
        '\u{00E7}' => ('c', '\u{0327}'),
        '\u{00E8}' => ('e', '\u{0300}'),
        '\u{00E9}' => ('e', '\u{0301}'),
        '\u{00EA}' => ('e', '\u{0302}'),
        '\u{00EB}' => ('e', '\u{0308}'),
        '\u{00EC}' => ('i', '\u{0300}'),
        '\u{00ED}' => ('i', '\u{0301}'),
        '\u{00EE}' => ('i', '\u{0302}'),
        '\u{00EF}' => ('i', '\u{0308}'),
        '\u{00F1}' => ('n', '\u{0303}'),
        '\u{00F2}' => ('o', '\u{0300}'),
        '\u{00F3}' => ('o', '\u{0301}'),
        '\u{00F4}' => ('o', '\u{0302}'),
        '\u{00F5}' => ('o', '\u{0303}'),
        '\u{00F6}' => ('o', '\u{0308}'),
        '\u{00F9}' => ('u', '\u{0300}'),
        '\u{00FA}' => ('u', '\u{0301}'),
        '\u{00FB}' => ('u', '\u{0302}'),
        '\u{00FC}' => ('u', '\u{0308}'),
        '\u{00FD}' => ('y', '\u{0301}'),
        '\u{00FF}' => ('y', '\u{0308}'),
        '\u{0100}' => ('A', '\u{0304}'),
        '\u{0101}' => ('a', '\u{0304}'),
        '\u{0102}' => ('A', '\u{0306}'),
        '\u{0103}' => ('a', '\u{0306}'),
        '\u{0104}' => ('A', '\u{0328}'),
        '\u{0105}' => ('a', '\u{0328}'),
        '\u{0106}' => ('C', '\u{0301}'),
        '\u{0107}' => ('c', '\u{0301}'),
        '\u{0108}' => ('C', '\u{0302}'),
        '\u{0109}' => ('c', '\u{0302}'),
        '\u{010A}' => ('C', '\u{0307}'),
        '\u{010B}' => ('c', '\u{0307}'),
        '\u{010C}' => ('C', '\u{030C}'),
        '\u{010D}' => ('c', '\u{030C}'),
        '\u{010E}' => ('D', '\u{030C}'),
        '\u{010F}' => ('d', '\u{030C}'),
        '\u{0112}' => ('E', '\u{0304}'),
        '\u{0113}' => ('e', '\u{0304}'),
        '\u{0114}' => ('E', '\u{0306}'),
        '\u{0115}' => ('e', '\u{0306}'),
        '\u{0116}' => ('E', '\u{0307}'),
        '\u{0117}' => ('e', '\u{0307}'),
        '\u{0118}' => ('E', '\u{0328}'),
        '\u{0119}' => ('e', '\u{0328}'),
        '\u{011A}' => ('E', '\u{030C}'),
        '\u{011B}' => ('e', '\u{030C}'),
        '\u{011C}' => ('G', '\u{0302}'),
        '\u{011D}' => ('g', '\u{0302}'),
        '\u{011E}' => ('G', '\u{0306}'),
        '\u{011F}' => ('g', '\u{0306}'),
        '\u{0120}' => ('G', '\u{0307}'),
        '\u{0121}' => ('g', '\u{0307}'),
        '\u{0122}' => ('G', '\u{0327}'),
        '\u{0123}' => ('g', '\u{0327}'),
        '\u{0124}' => ('H', '\u{0302}'),
        '\u{0125}' => ('h', '\u{0302}'),
        '\u{0128}' => ('I', '\u{0303}'),
        '\u{0129}' => ('i', '\u{0303}'),
        '\u{012A}' => ('I', '\u{0304}'),
        '\u{012B}' => ('i', '\u{0304}'),
        '\u{012C}' => ('I', '\u{0306}'),
        '\u{012D}' => ('i', '\u{0306}'),
        '\u{012E}' => ('I', '\u{0328}'),
        '\u{012F}' => ('i', '\u{0328}'),
        '\u{0130}' => ('I', '\u{0307}'),
        '\u{0134}' => ('J', '\u{0302}'),
        '\u{0135}' => ('j', '\u{0302}'),
        '\u{0136}' => ('K', '\u{0327}'),
        '\u{0137}' => ('k', '\u{0327}'),
        '\u{0139}' => ('L', '\u{0301}'),
        '\u{013A}' => ('l', '\u{0301}'),
        '\u{013B}' => ('L', '\u{0327}'),
        '\u{013C}' => ('l', '\u{0327}'),
        '\u{013D}' => ('L', '\u{030C}'),
        '\u{013E}' => ('l', '\u{030C}'),
        '\u{0143}' => ('N', '\u{0301}'),
        '\u{0144}' => ('n', '\u{0301}'),
        '\u{0145}' => ('N', '\u{0327}'),
        '\u{0146}' => ('n', '\u{0327}'),
        '\u{0147}' => ('N', '\u{030C}'),
        '\u{0148}' => ('n', '\u{030C}'),
        '\u{014C}' => ('O', '\u{0304}'),
        '\u{014D}' => ('o', '\u{0304}'),
        '\u{014E}' => ('O', '\u{0306}'),
        '\u{014F}' => ('o', '\u{0306}'),
        '\u{0150}' => ('O', '\u{030B}'),
        '\u{0151}' => ('o', '\u{030B}'),
        '\u{0154}' => ('R', '\u{0301}'),
        '\u{0155}' => ('r', '\u{0301}'),
        '\u{0156}' => ('R', '\u{0327}'),
        '\u{0157}' => ('r', '\u{0327}'),
        '\u{0158}' => ('R', '\u{030C}'),
        '\u{0159}' => ('r', '\u{030C}'),
        '\u{015A}' => ('S', '\u{0301}'),
        '\u{015B}' => ('s', '\u{0301}'),
        '\u{015C}' => ('S', '\u{0302}'),
        '\u{015D}' => ('s', '\u{0302}'),
        '\u{015E}' => ('S', '\u{0327}'),
        '\u{015F}' => ('s', '\u{0327}'),
        '\u{0160}' => ('S', '\u{030C}'),
        '\u{0161}' => ('s', '\u{030C}'),
        '\u{0162}' => ('T', '\u{0327}'),
        '\u{0163}' => ('t', '\u{0327}'),
        '\u{0164}' => ('T', '\u{030C}'),
        '\u{0165}' => ('t', '\u{030C}'),
        '\u{0168}' => ('U', '\u{0303}'),
        '\u{0169}' => ('u', '\u{0303}'),
        '\u{016A}' => ('U', '\u{0304}'),
        '\u{016B}' => ('u', '\u{0304}'),
        '\u{016C}' => ('U', '\u{0306}'),
        '\u{016D}' => ('u', '\u{0306}'),
        '\u{016E}' => ('U', '\u{030A}'),
        '\u{016F}' => ('u', '\u{030A}'),
        '\u{0170}' => ('U', '\u{030B}'),
        '\u{0171}' => ('u', '\u{030B}'),
        '\u{0172}' => ('U', '\u{0328}'),
        '\u{0173}' => ('u', '\u{0328}'),
        '\u{0174}' => ('W', '\u{0302}'),
        '\u{0175}' => ('w', '\u{0302}'),
        '\u{0176}' => ('Y', '\u{0302}'),
        '\u{0177}' => ('y', '\u{0302}'),
        '\u{0178}' => ('Y', '\u{0308}'),
        '\u{0179}' => ('Z', '\u{0301}'),
        '\u{017A}' => ('z', '\u{0301}'),
        '\u{017B}' => ('Z', '\u{0307}'),
        '\u{017C}' => ('z', '\u{0307}'),
        '\u{017D}' => ('Z', '\u{030C}'),
        '\u{017E}' => ('z', '\u{030C}'),
        _ => return None,
    })
}

fn compose_pair(starter: char, ch: char) -> Option<char> {
    let s = starter as u32;
    let c = ch as u32;
    if (0x1100..=0x1112).contains(&s) && (0x1161..=0x1175).contains(&c) {
        let l_index = s - 0x1100;
        let v_index = c - 0x1161;
        return char::from_u32(0xAC00 + (l_index * 21 + v_index) * 28);
    }
    if (0xAC00..=0xD7A3).contains(&s)
        && (s - 0xAC00).is_multiple_of(28)
        && (0x11A8..=0x11C2).contains(&c)
    {
        let t_index = c - 0x11A7;
        return char::from_u32(s + t_index);
    }
    for code in 0x00C0u32..=0x017Eu32 {
        if let Some(candidate) = char::from_u32(code)
            && canonical_pair(candidate) == Some((starter, ch))
        {
            return Some(candidate);
        }
    }
    None
}
