//! 命令输出的捕获、tail 截断与 UTF-8 lossy 解码。
//!
//! 语义：**保留最后 `maxBytes` 字节**（看错误尾巴最重要），被丢弃的前缀替换成
//! `[truncated N bytes]`。截断点会向回退到 UTF-8 字符边界，避免把多字节字符切碎。

/// 截断标记里用到的固定前后缀。
const TRUNCATED_PREFIX: &str = "[truncated ";

/// 输出缓冲：只保留尾部，追加成本恒定。
///
/// 追加时若超出上限，从**前端**丢弃最旧的数据（丢弃量累计到 `dropped`），
/// 因此内存占用永远不超过 `max_bytes + max_chunk`。
#[derive(Debug)]
pub struct TailBuffer {
    /// 已保留的字节（尾部）。
    data: Vec<u8>,
    /// 已丢弃的字节数（用于 `[truncated N bytes]` 标记）。
    dropped: u64,
    /// 上限字节数。
    max_bytes: usize,
}

impl TailBuffer {
    /// 创建缓冲。
    ///
    /// 参数：`max_bytes` 保留上限，必须 > 0（配置层已校验）。
    pub fn new(max_bytes: usize) -> Self {
        let max_bytes = max_bytes.max(1);
        Self {
            data: Vec::new(),
            dropped: 0,
            max_bytes,
        }
    }

    /// 追加一段输出；超出上限时丢弃最旧字节。
    ///
    /// 参数：`chunk` 新到达的字节（stdout 与 stderr 合并后直接喂进来）。
    pub fn append(&mut self, chunk: &[u8]) {
        // 第一步：单块就超过上限时，只需要保留它的尾部，前面全部计入丢弃。
        if chunk.len() >= self.max_bytes {
            let keep_from = chunk.len() - self.max_bytes;
            self.dropped = self.dropped.saturating_add(self.data.len() as u64);
            self.dropped = self.dropped.saturating_add(keep_from as u64);
            self.data.clear();
            self.data.extend_from_slice(&chunk[keep_from..]);
            return;
        }

        // 第二步：常规路径，腾出空间后整块追加。
        let overflow = (self.data.len() + chunk.len()).saturating_sub(self.max_bytes);
        if overflow > 0 {
            self.data.drain(..overflow);
            self.dropped = self.dropped.saturating_add(overflow as u64);
        }
        self.data.extend_from_slice(chunk);
    }

    /// 已丢弃的字节数。
    #[allow(dead_code)]
    pub fn dropped_bytes(&self) -> u64 {
        self.dropped
    }

    /// 当前是否发生过截断。
    pub fn is_truncated(&self) -> bool {
        self.dropped > 0
    }

    /// 当前保留的原始字节（尾部）。
    #[allow(dead_code)]
    pub fn retained(&self) -> &[u8] {
        &self.data
    }

    /// 渲染成给模型看的文本：必要时前置截断标记，并按 UTF-8 lossy 解码。
    ///
    /// 返回：`[truncated N bytes]` + 解码后的正文（未截断时没有标记）。
    /// 正文起点会回退到 UTF-8 字符边界，避免开头出现半个字符。
    pub fn render(&self) -> String {
        let body = decode_from_char_boundary(&self.data);
        if !self.is_truncated() {
            return body;
        }
        // 第一步：拼标记。标记本身是 ASCII，与正文编码无关。
        let mut out = String::with_capacity(body.len() + 32);
        out.push_str(TRUNCATED_PREFIX);
        out.push_str(&self.dropped.to_string());
        out.push_str(" bytes]\n");
        out.push_str(&body);
        out
    }

    /// 渲染成没有截断标记的裸正文（用于 `list_cmds` 等只展示摘要的场景）。
    #[allow(dead_code)]
    pub fn render_body(&self) -> String {
        decode_from_char_boundary(&self.data)
    }
}

/// 从尾部字节里找第一个合法 UTF-8 边界再解码。
///
/// 参数：`data` 原始字节。
/// 返回：lossy 解码后的字符串；起点最多向前跳过 3 个字节（UTF-8 单字符上限）。
fn decode_from_char_boundary(data: &[u8]) -> String {
    // 第一步：先直接尝试整体解码，绝大多数情况（截断点正好在边界）一次成功。
    if let Ok(text) = std::str::from_utf8(data) {
        return text.to_string();
    }

    // 第二步：从第 0 字节开始向后找第一个字符边界（最多跳 3 字节）。
    let limit = data.len().min(4);
    for skip in 1..limit {
        if let Ok(text) = std::str::from_utf8(&data[skip..]) {
            return text.to_string();
        }
    }

    // 第三步：实在找不到边界（数据本身就不是 UTF-8），交给 lossy 兜底。
    String::from_utf8_lossy(data).into_owned()
}

#[cfg(test)]
mod tests {
    use super::TailBuffer;

    #[test]
    fn keeps_everything_when_under_limit() {
        let mut buffer = TailBuffer::new(64);
        buffer.append(b"hello ");
        buffer.append(b"world");
        assert!(!buffer.is_truncated());
        assert_eq!(buffer.render(), "hello world");
        assert_eq!(buffer.dropped_bytes(), 0);
    }

    #[test]
    fn keeps_the_tail_and_marks_dropped_bytes() {
        let mut buffer = TailBuffer::new(10);
        buffer.append(b"0123456789ABCDEF");
        // 最后 10 字节 = "6789ABCDEF"，丢弃 6 字节。
        assert_eq!(buffer.retained(), b"6789ABCDEF");
        assert_eq!(buffer.dropped_bytes(), 6);
        let rendered = buffer.render();
        assert!(rendered.starts_with("[truncated 6 bytes]"), "{rendered}");
        assert!(rendered.ends_with("6789ABCDEF"), "{rendered}");
    }

    #[test]
    fn single_oversized_chunk_keeps_only_its_tail() {
        let mut buffer = TailBuffer::new(4);
        buffer.append(b"aa");
        buffer.append(b"bbbbbbbb");
        assert_eq!(buffer.retained(), b"bbbb");
        assert_eq!(buffer.dropped_bytes(), 6);
    }

    #[test]
    fn truncation_never_splits_a_utf8_character() {
        // 每个中文字符 3 字节；上限 4 字节会切在字符中间。
        let mut buffer = TailBuffer::new(4);
        buffer.append("中文".as_bytes());
        let rendered = buffer.render();
        // 不出现替换字符，且正文是完整的「文」。
        assert!(!rendered.contains('\u{FFFD}'), "{rendered}");
        assert!(rendered.ends_with('文'), "{rendered}");
    }

    #[test]
    fn invalid_utf8_falls_back_to_lossy() {
        let mut buffer = TailBuffer::new(16);
        buffer.append(&[0x80, 0xff, 0xfe]);
        let rendered = buffer.render();
        assert!(rendered.contains('\u{FFFD}'), "{rendered}");
    }

    #[test]
    fn dropped_bytes_accumulate_across_appends() {
        let mut buffer = TailBuffer::new(5);
        for chunk in [b"abc".as_slice(), b"def", b"ghi"] {
            buffer.append(chunk);
        }
        // 总共 9 字节，保留 5，丢弃 4。
        assert_eq!(buffer.retained(), b"efghi");
        assert_eq!(buffer.dropped_bytes(), 4);
    }
}
