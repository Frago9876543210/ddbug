use std::io::Write;

use marksman_escape::Escape;

use {Options, Result};
use super::{DiffPrefix, Printer};

const HEADER: &str = r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8"/>
<style>
ul.collapsible {
    list-style: none;
}
ul.collapsible ul {
    display: none;
    list-style: none;
}
ul.collapsible li {
    white-space: nowrap;
    cursor: default;
}
ul.collapsible li.collapsible {
    cursor: pointer;
    border: 1px solid black;
}
ul.collapsible span {
    white-space: normal;
}
ul.collapsible .del {
    background-color: #ffdce0;
}
ul.collapsible .add {
    background-color: #cdffd8;
}
span.field {
    display: inline-block;
    vertical-align: top;
}
</style>
<script language="javascript">
window.onload = function () {
    var uls = document.querySelectorAll(".collapsible li > ul");
    [].forEach.call(uls, function(ul) {
        var li = ul.parentNode;
        li.classList.add("collapsible");
        li.addEventListener("click", toggle);
    });

    function toggle (e) {
        var node = e.target;
        while (node.nodeName !== "LI") {
            node = node.parentNode;
        }
        if (node === e.currentTarget) {
            var uls = node.getElementsByTagName("ul");
            [].forEach.call(uls, function(ul) {
                var li = ul;
                while (li !== null && li.nodeName !== "LI") {
                    li = li.parentNode;
                }
                if (li === node) {
                    if (ul.style.display == "block") {
                        ul.style.display = "none";
                    } else {
                        ul.style.display = "block";
                    }
                }
            });
        }
    }
}
</script>
</head>
<body>
<ul class="collapsible">"#;

const FOOTER: &str = r#"</ul>
</body>
</html>"#;

pub struct HtmlPrinter<'w> {
    w: &'w mut Write,
    indent: usize,
    prefix: DiffPrefix,
    inline_depth: usize,
    // Hack to allow indented <ul> to be included within parent <li>.
    line_started: bool,
}

impl<'w> HtmlPrinter<'w> {
    pub fn new(w: &'w mut Write, options: &Options) -> Self {
        HtmlPrinter {
            w,
            indent: 0,
            prefix: DiffPrefix::None,
            inline_depth: options.inline_depth,
            line_started: false,
        }
    }

    pub fn begin(&mut self) -> Result<()> {
        self.w.write_all(HEADER.as_bytes())?;
        Ok(())
    }

    pub fn end(&mut self) -> Result<()> {
        if self.line_started {
            writeln!(self.w, "</li>")?;
        }
        self.w.write_all(FOOTER.as_bytes())?;
        Ok(())
    }
}

impl<'w> Printer for HtmlPrinter<'w> {
    /// Calls `f` to write to a temporary buffer.
    fn buffer(
        &mut self,
        buf: &mut Vec<u8>,
        f: &mut FnMut(&mut Printer) -> Result<()>,
    ) -> Result<()> {
        let mut p = HtmlPrinter {
            w: buf,
            // TODO: need to ensure these are all unchanged?
            indent: self.indent,
            prefix: self.prefix,
            inline_depth: self.inline_depth,
            line_started: self.line_started,
        };
        f(&mut p)?;

        if self.line_started != p.line_started {
            if p.line_started {
                writeln!(p.w, "</li>")?;
            } else {
                write!(p.w, "<li>")?;
            }
        }
        Ok(())
    }

    fn write_buf(&mut self, buf: &[u8]) -> Result<()> {
        self.w.write_all(buf)?;
        Ok(())
    }

    fn line_break(&mut self) -> Result<()> {
        Ok(())
    }

    fn line(&mut self, label: &str, buf: &[u8]) -> Result<()> {
        if self.line_started {
            writeln!(self.w, "</li>")?;
        }
        write!(self.w, "<li>")?;
        self.line_started = true;
        if buf.is_empty() {
            write!(self.w, "{}:", label)?;
            return Ok(());
        }
        if !label.is_empty() {
            write!(self.w, "<span class=\"field\">{}:</span> <span class=\"field\">", label)?;
        }
        match self.prefix {
            DiffPrefix::None | DiffPrefix::Equal => write!(self.w, "<span>")?,
            DiffPrefix::Less => {
                write!(self.w, "<span class=\"del\">")?;
            }
            DiffPrefix::Greater => {
                write!(self.w, "</span>\n<span class=\"add\">")?;
            }
        }
        self.w.write_all(&*escaped(buf))?;
        write!(self.w, "</span>")?;
        if !label.is_empty() {
            write!(self.w, "</span>")?;
        }
        Ok(())
    }

    fn line_diff(&mut self, label: &str, a: &[u8], b: &[u8]) -> Result<()> {
        if self.line_started {
            writeln!(self.w, "</li>")?;
        }
        write!(self.w, "<li>")?;
        self.line_started = true;
        if !label.is_empty() {
            write!(self.w, "<span class=\"field\">{}:</span> <span class=\"field\">", label)?;
        }
        write!(self.w, "<span class=\"del\">")?;
        self.w.write_all(&*escaped(a))?;
        write!(self.w, "</span><br>\n<span class=\"add\">")?;
        self.w.write_all(&*escaped(b))?;
        write!(self.w, "</span>")?;
        if !label.is_empty() {
            write!(self.w, "</span>")?;
        }
        Ok(())
    }

    fn indent(&mut self, body: &mut FnMut(&mut Printer) -> Result<()>) -> Result<()> {
        if !self.line_started {
            write!(self.w, "<li>")?;
        }
        self.line_started = false;
        self.indent += 1;
        writeln!(self.w, "<ul>")?;

        // TODO: omit <ul></ul> if body is empty
        body(self)?;

        if self.line_started {
            writeln!(self.w, "</li>")?;
        }
        self.line_started = true;
        self.indent -= 1;
        write!(self.w, "</ul>")?;
        Ok(())
    }

    fn prefix(&mut self, prefix: DiffPrefix) {
        self.prefix = prefix;
    }

    fn inline_begin(&mut self) -> bool {
        if self.inline_depth == 0 {
            false
        } else {
            self.inline_depth -= 1;
            true
        }
    }

    fn inline_end(&mut self) {
        self.inline_depth += 1;
    }
}

fn escaped(bytes: &[u8]) -> Vec<u8> {
    Escape::new(bytes.iter().map(|x| *x)).collect()
}
