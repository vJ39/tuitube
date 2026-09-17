const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
/// APC 1 個の上限。壊れたストリームを延々と溜め込まないための足切り。
const MAX_APC_LEN: usize = 8192;
/// 組み立て中フレームの上限。MAX_APC_LEN は APC 1 個しか縛らないので、
/// m=0 が来ないストリームで累積側が伸び続けないよう同じ足切りを入れる。
/// MAX_FRAME_PIXELS を f=24(3 バイト/px)で base64 化した長さの 2 倍。
const MAX_FRAME_LEN: usize = crate::video::MAX_FRAME_PIXELS as usize * 3 * 4 / 3 * 2;
const APC_INTRO: &[u8] = b"\x1b_G";
/// APC の終端 (String Terminator)。チャンクの切れ目でもある。
pub const ST: &[u8] = b"\x1b\\";

/// 完結した APC G コマンド 1 個。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphicsCommand {
    /// 制御部 "a=T,f=24,s=320,..." を (キー, 値) に分解したもの。順序保持。
    pub keys: Vec<(char, String)>,
    /// ESC _ G から ESC \ までの完全なバイト列。ST 欠落は付け直し済み。
    pub raw: Vec<u8>,
}

impl GraphicsCommand {
    pub fn get(&self, key: char) -> Option<&str> {
        self.keys
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Default, Clone, Copy)]
enum State {
    #[default]
    Ground,
    Esc,
    Csi,
    /// ESC _ を見た。次の 1 バイトが G かどうかで扱いが変わる。
    ApcIntro,
    Apc,
    /// APC 中の ESC。ST の前半かもしれないし、mpv が ST を落とした打ち切りかもしれない。
    ApcEsc,
    /// G 以外の APC・OSC・DCS など。終端まで読み飛ばす。
    Skip,
}

/// 分断された read を跨いで状態を持つストリームパーサ。APC G 以外は全て捨てる。
#[derive(Default)]
pub struct ApcParser {
    state: State,
    buf: Vec<u8>,
}

impl ApcParser {
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<GraphicsCommand>) {
        for &byte in input {
            self.step(byte, out);
        }
    }

    fn step(&mut self, byte: u8, out: &mut Vec<GraphicsCommand>) {
        match self.state {
            State::Ground => {
                if byte == ESC {
                    self.state = State::Esc;
                }
            }
            State::Esc => {
                self.state = match byte {
                    b'[' => State::Csi,
                    b'_' => State::ApcIntro,
                    b']' | b'P' | b'X' | b'^' => State::Skip,
                    ESC => State::Esc,
                    _ => State::Ground,
                }
            }
            // パラメータ・中間バイトを読み飛ばし、終端バイトで抜ける。
            State::Csi => {
                self.state = match byte {
                    0x20..=0x3f => State::Csi,
                    ESC => State::Esc,
                    _ => State::Ground,
                }
            }
            State::ApcIntro => {
                self.state = match byte {
                    b'G' => {
                        self.buf.clear();
                        self.buf.extend_from_slice(APC_INTRO);
                        State::Apc
                    }
                    ESC => State::Esc,
                    _ => State::Skip,
                }
            }
            State::Apc => {
                if byte == ESC {
                    self.state = State::ApcEsc;
                } else if self.buf.len() >= MAX_APC_LEN {
                    self.buf.clear();
                    self.state = State::Skip;
                } else {
                    self.buf.push(byte);
                }
            }
            State::ApcEsc => {
                self.finish(out);
                // ST でも素の ESC でも 1 個として完結させ、打ち切った ESC は次の列の頭にする。
                self.state = State::Ground;
                if byte != b'\\' {
                    self.state = State::Esc;
                    self.step(byte, out);
                }
            }
            State::Skip => {
                self.state = match byte {
                    ESC => State::Esc,
                    BEL => State::Ground,
                    _ => State::Skip,
                }
            }
        }
    }

    fn finish(&mut self, out: &mut Vec<GraphicsCommand>) {
        let mut raw = std::mem::take(&mut self.buf);
        raw.extend_from_slice(ST);
        out.push(GraphicsCommand {
            keys: parse_keys(&raw),
            raw,
        });
    }
}

/// raw から ESC _ G と ST を外し、";" より手前の制御部を (キー, 値) へ分解する。
fn parse_keys(raw: &[u8]) -> Vec<(char, String)> {
    let body = &raw[APC_INTRO.len()..raw.len() - ST.len()];
    let control = match body.iter().position(|b| *b == b';') {
        Some(end) => &body[..end],
        None => body,
    };
    control
        .split(|b| *b == b',')
        .filter_map(|pair| {
            let mut parts = pair.splitn(2, |b| *b == b'=');
            let key = parts.next()?;
            let value = parts.next()?;
            match key {
                [k] if k.is_ascii_alphabetic() => {
                    Some((*k as char, String::from_utf8_lossy(value).into_owned()))
                }
                _ => None,
            }
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrame {
    pub width_px: u32,
    pub height_px: u32,
    /// 先頭チャンクから m=0 チャンクまでの raw を連結したもの。
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameEvent {
    Frame(VideoFrame),
    /// a=d を見た。未完成フレームは捨てる。
    Clear,
}

#[derive(Default)]
pub struct FrameAssembler {
    open: Option<VideoFrame>,
}

impl FrameAssembler {
    pub fn push(&mut self, cmd: GraphicsCommand) -> Option<FrameEvent> {
        match cmd.get('a') {
            Some("T") => {
                self.open = None;
                let (Some(width_px), Some(height_px)) = (number(&cmd, 's'), number(&cmd, 'v'))
                else {
                    return None;
                };
                // チャンク分割されない 1 個だけのコマンドはその場で 1 フレーム。
                let done = matches!(cmd.get('m'), None | Some("0"));
                let frame = VideoFrame {
                    width_px,
                    height_px,
                    bytes: cmd.raw,
                };
                if done {
                    return Some(FrameEvent::Frame(frame));
                }
                self.open = Some(frame);
                None
            }
            Some("d") => {
                self.open = None;
                Some(FrameEvent::Clear)
            }
            Some(_) => None,
            None => {
                let last = cmd.get('m')? == "0";
                let open = self.open.as_mut()?;
                if open.bytes.len() + cmd.raw.len() > MAX_FRAME_LEN {
                    self.open = None;
                    return None;
                }
                open.bytes.extend_from_slice(&cmd.raw);
                if last {
                    self.open.take().map(FrameEvent::Frame)
                } else {
                    None
                }
            }
        }
    }
}

fn number(cmd: &GraphicsCommand, key: char) -> Option<u32> {
    cmd.get(key)?.parse().ok()
}

#[cfg(test)]
pub(crate) mod fixtures {
    /// alt-screen=no のときの起動列。
    pub const KITTY_PROLOGUE: &[u8] = b"\x1b[?25l\x1b[?1003h";
    /// config-clear=no のときの構成列。
    pub const KITTY_RECONFIG: &[u8] = b"\x1b_Ga=d;\x1b\\";
    /// 終了列。a=d に ST が無い・最後の HVP に cols(80) が入るのが mpv の実装どおり。
    pub const KITTY_UNINIT: &[u8] = b"\x1b_Ga=d;\x1b[?25h\x1b[?1003l\x1b[80;0f";

    const CHUNK: usize = 4096;

    /// vo_kitty.c の flip_page と同じ形: HVP + 先頭チャンク(m=1) + 継続 + 最終(m=0)。
    pub fn frame(s: u32, v: u32, data: &[u8]) -> Vec<u8> {
        let mut chunks: Vec<&[u8]> = data.chunks(CHUNK).collect();
        if chunks.len() < 2 {
            chunks.resize(1, data);
            chunks.push(&[]);
        }
        let mut out = b"\x1b[1;1f".to_vec();
        let last = chunks.len() - 1;
        for (i, chunk) in chunks.iter().enumerate() {
            let head = if i == 0 {
                format!("\x1b_Ga=T,f=24,s={s},v={v},C=1,q=2,m=1;")
            } else {
                format!("\x1b_Gm={};", u8::from(i != last))
            };
            out.extend_from_slice(head.as_bytes());
            out.extend_from_slice(chunk);
            out.extend_from_slice(b"\x1b\\");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{KITTY_PROLOGUE, KITTY_RECONFIG, KITTY_UNINIT, frame};
    use super::*;

    fn parse(input: &[u8]) -> Vec<GraphicsCommand> {
        let mut parser = ApcParser::default();
        let mut out = Vec::new();
        parser.feed(input, &mut out);
        out
    }

    const SINGLE: &[u8] = b"\x1b_Ga=T,f=24,s=2,v=1,C=1,q=2,m=0;AAAAAAAA\x1b\\";

    fn mixed_stream() -> Vec<u8> {
        [
            KITTY_PROLOGUE,
            KITTY_RECONFIG,
            b"\x1b[1;1f",
            SINGLE,
            b"\x1b[0m\n",
        ]
        .concat()
    }

    #[test]
    fn apc_parser_keeps_graphics_commands_and_drops_everything_else() {
        let commands = parse(&mixed_stream());
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].get('a'), Some("d"));
        assert_eq!(commands[0].raw, KITTY_RECONFIG);
        assert_eq!(commands[1].get('a'), Some("T"));
        assert_eq!(commands[1].get('s'), Some("2"));
        assert_eq!(commands[1].get('v'), Some("1"));
        assert_eq!(commands[1].get('m'), Some("0"));
        assert_eq!(commands[1].raw, SINGLE);
        // 制御部に無いキーは None。
        assert_eq!(commands[1].get('i'), None);
    }

    #[test]
    fn apc_parser_gives_the_same_result_for_every_split_point() {
        let input = mixed_stream();
        let whole = parse(&input);
        for split in 0..=input.len() {
            let mut parser = ApcParser::default();
            let mut out = Vec::new();
            parser.feed(&input[..split], &mut out);
            parser.feed(&input[split..], &mut out);
            assert_eq!(out, whole, "split at {split}");
        }
        let mut parser = ApcParser::default();
        let mut out = Vec::new();
        for byte in &input {
            parser.feed(&[*byte], &mut out);
        }
        assert_eq!(out, whole, "byte by byte");
    }

    #[test]
    fn assembler_joins_chunks_until_the_final_one() {
        let data = vec![b'Q'; 9000];
        let commands = parse(&frame(320, 180, &data));
        assert_eq!(commands.len(), 3);
        let joined: Vec<u8> = commands.iter().flat_map(|c| c.raw.clone()).collect();

        let mut assembler = FrameAssembler::default();
        let mut events: Vec<Option<FrameEvent>> = Vec::new();
        for cmd in commands {
            events.push(assembler.push(cmd));
        }
        assert_eq!(events[0], None);
        assert_eq!(events[1], None);
        assert_eq!(
            events[2],
            Some(FrameEvent::Frame(VideoFrame {
                width_px: 320,
                height_px: 180,
                bytes: joined,
            }))
        );
        // HVP は取り込まない。
        assert!(!matches!(&events[2], Some(FrameEvent::Frame(f)) if f.bytes.starts_with(b"\x1b[")));
    }

    #[test]
    fn bare_esc_terminated_delete_becomes_clear_without_eating_the_next_sequence() {
        let commands = parse(&[KITTY_UNINIT, &frame(2, 1, b"AAAAAAAA")].concat());
        let mut assembler = FrameAssembler::default();
        let mut events = commands.into_iter().filter_map(|c| assembler.push(c));
        assert_eq!(events.next(), Some(FrameEvent::Clear));
        let Some(FrameEvent::Frame(frame)) = events.next() else {
            panic!("frame must follow the clear");
        };
        assert_eq!((frame.width_px, frame.height_px), (2, 1));
        assert!(frame.bytes.starts_with(b"\x1b_Ga=T,"));
        assert_eq!(events.next(), None);
    }

    #[test]
    fn assembler_discards_an_unfinished_frame_when_a_new_one_starts() {
        // m=0 チャンクが出ないまま次のフレームが始まる (mpv の小画像バグ / VO 作り直し)。
        let mut stream = b"\x1b_Ga=T,f=24,s=4,v=2,C=1,q=2,m=1;XXXXXXXX\x1b\\".to_vec();
        stream.extend_from_slice(&frame(2, 1, b"YYYY"));

        let mut assembler = FrameAssembler::default();
        let events: Vec<FrameEvent> = parse(&stream)
            .into_iter()
            .filter_map(|c| assembler.push(c))
            .collect();
        assert_eq!(events.len(), 1);
        let FrameEvent::Frame(frame) = &events[0] else {
            panic!("expected a frame");
        };
        assert_eq!((frame.width_px, frame.height_px), (2, 1));
        assert!(!frame.bytes.contains(&b'X'), "捨てたフレームが混ざっている");
    }

    /// 4096 バイトぶんの継続チャンク 1 個。
    fn continuation(last: bool) -> GraphicsCommand {
        let mut raw = format!("\x1b_Gm={};", u8::from(!last)).into_bytes();
        raw.extend(std::iter::repeat_n(b'Q', 4096));
        raw.extend_from_slice(ST);
        GraphicsCommand {
            keys: parse_keys(&raw),
            raw,
        }
    }

    #[test]
    fn assembler_drops_a_frame_that_never_closes() {
        let head = parse(b"\x1b_Ga=T,f=24,s=320,v=176,C=1,q=2,m=1;AAAA\x1b\\")
            .pop()
            .expect("先頭チャンク");
        let mut assembler = FrameAssembler::default();
        assert_eq!(assembler.push(head), None);

        // m=0 が来ないまま 8.2MB ぶんの継続チャンクが届いても、上限までしか溜めない。
        for _ in 0..2000 {
            assert_eq!(assembler.push(continuation(false)), None);
            let held = assembler.open.as_ref().map_or(0, |f| f.bytes.len());
            assert!(held <= MAX_FRAME_LEN, "{held} バイト溜め込んでいる");
        }
        assert!(assembler.open.is_none(), "上限超過のフレームが残っている");
        // 捨てた後の m=0 は開いていないフレームへの継続なので何も生まない。
        assert_eq!(assembler.push(continuation(true)), None);
    }

    #[test]
    fn continuation_without_an_open_frame_is_ignored() {
        let mut assembler = FrameAssembler::default();
        let events: Vec<FrameEvent> = parse(b"\x1b_Gm=1;AAAA\x1b\\\x1b_Gm=0;BBBB\x1b\\")
            .into_iter()
            .filter_map(|c| assembler.push(c))
            .collect();
        assert!(events.is_empty());
    }

    #[test]
    fn oversized_apc_is_dropped_and_parsing_resumes() {
        let mut stream = b"\x1b_Ga=T,f=24,s=2,v=1,C=1,q=2,m=1;".to_vec();
        stream.extend(std::iter::repeat_n(b'Z', 9000));
        stream.extend_from_slice(b"\x1b\\");
        stream.extend_from_slice(SINGLE);

        let commands = parse(&stream);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].raw, SINGLE);
    }

    #[test]
    fn a_frame_opened_without_a_size_is_ignored() {
        let mut assembler = FrameAssembler::default();
        let events: Vec<FrameEvent> = parse(b"\x1b_Ga=T,f=24,C=1,q=2,m=0;AAAA\x1b\\")
            .into_iter()
            .filter_map(|c| assembler.push(c))
            .collect();
        assert!(events.is_empty());
    }
}
