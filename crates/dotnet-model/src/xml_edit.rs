//! Small edits to XML files (projects, `.props`, `.slnx`) that leave everything else as it
//! was: comments, attribute order, indentation and line endings. The document is parsed
//! with roxmltree only to find byte ranges; changes are splices of the original text.

use std::ops::Range;

use anyhow::{Context as _, Result};

/// An element as found in the text, detached from the parse so the text can change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Element {
    /// Byte range of the whole element, from `<` to the end of its closing tag.
    pub range: Range<usize>,
    pub name: String,
    pub attributes: Vec<(String, String)>,
    /// Text of the element when it has no child elements, trimmed.
    pub text: Option<String>,
    /// Starts of the ancestors' ranges, nearest first.
    pub ancestors: Vec<usize>,
    pub depth: usize,
}

impl Element {
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attributes.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
    }

    pub fn start(&self) -> usize {
        self.range.start
    }

    pub fn parent(&self) -> Option<usize> {
        self.ancestors.first().copied()
    }
}

/// A new element to insert, with attributes in the given order.
#[derive(Clone, Debug, Default)]
pub struct NewElement {
    pub name: String,
    pub attributes: Vec<(String, String)>,
    pub text: Option<String>,
    pub children: Vec<NewElement>,
}

impl NewElement {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), ..Default::default() }
    }

    pub fn attr(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.attributes.push((name.into(), value.into()));
        self
    }

    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }

    pub fn child(mut self, child: NewElement) -> Self {
        self.children.push(child);
        self
    }

    fn render(&self, indent: &str, unit: &str, newline: &str) -> String {
        let mut out = format!("<{}", self.name);
        for (key, value) in &self.attributes {
            out.push_str(&format!(" {key}=\"{}\"", escape(value)));
        }
        if let Some(text) = &self.text {
            out.push_str(&format!(">{}</{}>", escape(text), self.name));
        } else if self.children.is_empty() {
            out.push_str(" />");
        } else {
            out.push('>');
            let inner = format!("{indent}{unit}");
            for child in &self.children {
                out.push_str(newline);
                out.push_str(&inner);
                out.push_str(&child.render(&inner, unit, newline));
            }
            out.push_str(&format!("{newline}{indent}</{}>", self.name));
        }
        out
    }
}

pub fn escape(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The text of an XML document being edited.
#[derive(Clone, Debug)]
pub struct XmlText {
    text: String,
}

impl XmlText {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn into_string(self) -> String {
        self.text
    }

    fn parse(&self) -> Result<roxmltree::Document<'_>> {
        let options = roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() };
        roxmltree::Document::parse_with_options(&self.text, options).context("not valid XML")
    }

    /// Every element in document order.
    pub fn elements(&self) -> Result<Vec<Element>> {
        let doc = self.parse()?;
        Ok(doc
            .descendants()
            .filter(|node| node.is_element())
            .map(|node| {
                let ancestors: Vec<usize> = node.ancestors().skip(1).filter(|a| a.is_element()).map(|a| a.range().start).collect();
                let has_child_elements = node.children().any(|child| child.is_element());
                Element {
                    range: node.range(),
                    name: node.tag_name().name().to_string(),
                    attributes: node.attributes().map(|a| (a.name().to_string(), a.value().to_string())).collect(),
                    text: (!has_child_elements).then(|| node.text().unwrap_or("").trim().to_string()).filter(|t| !t.is_empty()),
                    depth: ancestors.len(),
                    ancestors,
                }
            })
            .collect())
    }

    pub fn root(&self) -> Result<Element> {
        self.elements()?.into_iter().next().context("empty XML document")
    }

    /// The element whose range starts at `start`.
    pub fn element_at(&self, start: usize) -> Result<Element> {
        self.elements()?.into_iter().find(|e| e.range.start == start).context("element not found")
    }

    /// Direct child elements of the element starting at `parent`.
    pub fn children(&self, parent: usize) -> Result<Vec<Element>> {
        Ok(self.elements()?.into_iter().filter(|e| e.parent() == Some(parent)).collect())
    }

    pub fn newline(&self) -> &'static str {
        if self.text.contains("\r\n") { "\r\n" } else { "\n" }
    }

    /// The indentation step the file uses, from its first indented element.
    pub fn indent_unit(&self) -> String {
        let Ok(elements) = self.elements() else { return "  ".into() };
        elements
            .iter()
            .find(|e| e.depth == 1)
            .map(|e| self.line_indent(e.range.start))
            .filter(|indent| !indent.is_empty())
            .unwrap_or_else(|| "  ".into())
    }

    /// The whitespace before `pos` on its line, when only whitespace precedes it.
    fn line_indent(&self, pos: usize) -> String {
        let line_start = self.text[..pos].rfind('\n').map_or(0, |i| i + 1);
        let prefix = &self.text[line_start..pos];
        if prefix.chars().all(|c| c == ' ' || c == '\t') { prefix.to_string() } else { String::new() }
    }

    fn splice(&mut self, range: Range<usize>, with: &str) {
        self.text.replace_range(range, with);
    }

    /// Sets an attribute, adding it after the existing ones when missing.
    pub fn set_attribute(&mut self, element: usize, name: &str, value: &str) -> Result<()> {
        let (value_range, insert_at) = {
            let doc = self.parse()?;
            let node = doc.descendants().find(|n| n.is_element() && n.range().start == element).context("element not found")?;
            let existing = node.attributes().find(|a| a.name().eq_ignore_ascii_case(name)).map(|a| a.range_value());
            (existing, self.start_tag_end(node.range()))
        };
        match value_range {
            Some(range) => self.splice(range, &escape(value)),
            None => self.splice(insert_at..insert_at, &format!(" {name}=\"{}\"", escape(value))),
        }
        Ok(())
    }

    /// Removes an attribute and the space before it.
    pub fn remove_attribute(&mut self, element: usize, name: &str) -> Result<bool> {
        let range = {
            let doc = self.parse()?;
            let node = doc.descendants().find(|n| n.is_element() && n.range().start == element).context("element not found")?;
            node.attributes().find(|a| a.name().eq_ignore_ascii_case(name)).map(|a| a.range())
        };
        let Some(mut range) = range else { return Ok(false) };
        while range.start > 0 && self.text.as_bytes()[range.start - 1].is_ascii_whitespace() {
            range.start -= 1;
        }
        self.splice(range, "");
        Ok(true)
    }

    /// Replaces the text content of an element that has no children.
    pub fn set_text(&mut self, element: usize, value: &str) -> Result<()> {
        let el = self.element_at(element)?;
        let start_end = self.start_tag_end(el.range.clone());
        let raw = &self.text[el.range.clone()];
        if raw.ends_with("/>") {
            let replacement = format!("<{0}{1}>{2}</{0}>", el.name, &self.text[el.range.start + 1 + el.name.len()..start_end], escape(value));
            self.splice(el.range, &replacement);
        } else {
            let close = el.range.start + raw.rfind("</").context("no closing tag")?;
            self.splice(start_end + 1..close, &escape(value));
        }
        Ok(())
    }

    /// Position of the `>` or `/>` that ends a start tag, skipping quoted attribute values.
    fn start_tag_end(&self, range: Range<usize>) -> usize {
        let bytes = self.text.as_bytes();
        let mut quote = None;
        let mut i = range.start;
        while i < range.end {
            let c = bytes[i];
            match quote {
                Some(q) if c == q => quote = None,
                Some(_) => {}
                None if c == b'"' || c == b'\'' => quote = Some(c),
                None if c == b'>' => return if i > 0 && bytes[i - 1] == b'/' { i - 1 } else { i },
                None => {}
            }
            i += 1;
        }
        range.end
    }

    /// Removes an element, and its line when nothing else is on it.
    pub fn remove_element(&mut self, element: usize) -> Result<()> {
        let el = self.element_at(element)?;
        let range = self.whole_lines(el.range);
        self.splice(range, "");
        Ok(())
    }

    /// Grows a range to whole lines when only whitespace surrounds it on them.
    fn whole_lines(&self, range: Range<usize>) -> Range<usize> {
        let bytes = self.text.as_bytes();
        let mut start = range.start;
        while start > 0 && (bytes[start - 1] == b' ' || bytes[start - 1] == b'\t') {
            start -= 1;
        }
        let at_line_start = start == 0 || bytes[start - 1] == b'\n';
        let mut end = range.end;
        while end < bytes.len() && (bytes[end] == b' ' || bytes[end] == b'\t' || bytes[end] == b'\r') {
            end += 1;
        }
        let at_line_end = end == bytes.len() || bytes[end] == b'\n';
        if at_line_start && at_line_end {
            start..(end + 1).min(bytes.len())
        } else {
            range
        }
    }

    /// Adds `child` as the last child of `parent`, indented one step deeper.
    pub fn append_child(&mut self, parent: usize, child: &NewElement) -> Result<usize> {
        let el = self.element_at(parent)?;
        let newline = self.newline();
        let unit = self.indent_unit();
        let parent_indent = self.line_indent(el.range.start);
        let child_indent = format!("{parent_indent}{unit}");
        let rendered = child.render(&child_indent, &unit, newline);
        let raw = &self.text[el.range.clone()];
        if raw.ends_with("/>") {
            let start_end = self.start_tag_end(el.range.clone());
            let head = self.text[el.range.start..start_end].trim_end().to_string();
            let replacement = format!("{head}>{newline}{child_indent}{rendered}{newline}{parent_indent}</{}>", el.name);
            let offset = head.len() + 1 + newline.len() + child_indent.len();
            let at = el.range.start;
            self.splice(el.range, &replacement);
            return Ok(at + offset);
        }
        let close = el.range.start + raw.rfind("</").context("no closing tag")?;
        let line_start = self.text[..close].rfind('\n').map_or(0, |i| i + 1);
        if self.text[line_start..close].chars().all(|c| c == ' ' || c == '\t') && line_start > el.range.start {
            let insertion = format!("{child_indent}{rendered}{newline}");
            self.splice(line_start..line_start, &insertion);
            Ok(line_start + child_indent.len())
        } else {
            let insertion = format!("{newline}{child_indent}{rendered}{newline}{parent_indent}");
            self.splice(close..close, &insertion);
            Ok(close + newline.len() + child_indent.len())
        }
    }

    /// Adds `new` right after the sibling element starting at `sibling`, on its own line.
    pub fn insert_after(&mut self, sibling: usize, new: &NewElement) -> Result<usize> {
        let el = self.element_at(sibling)?;
        let newline = self.newline();
        let unit = self.indent_unit();
        let indent = self.line_indent(el.range.start);
        let rendered = new.render(&indent, &unit, newline);
        let at = el.range.end;
        self.splice(at..at, &format!("{newline}{indent}{rendered}"));
        Ok(at + newline.len() + indent.len())
    }

    /// Adds `new` right before the sibling element starting at `sibling`, on its own line.
    pub fn insert_before(&mut self, sibling: usize, new: &NewElement) -> Result<usize> {
        let el = self.element_at(sibling)?;
        let newline = self.newline();
        let unit = self.indent_unit();
        let indent = self.line_indent(el.range.start);
        let rendered = new.render(&indent, &unit, newline);
        let at = el.range.start;
        self.splice(at..at, &format!("{rendered}{newline}{indent}"));
        Ok(at)
    }

    /// Moves an element (with its line) to the end of another parent.
    pub fn move_element(&mut self, element: usize, new_parent: usize) -> Result<()> {
        let el = self.element_at(element)?;
        let source = self.text[el.range.clone()].to_string();
        let parent = self.element_at(new_parent)?;
        // Insert first at a marker, then remove: offsets after the removed range shift.
        let marker = NewElement::new("__forge_move_marker__");
        self.append_child(parent.range.start, &marker)?;
        let el_start = if parent.range.start < el.range.start { self.find_moved(&source, el.range.start)? } else { el.range.start };
        self.remove_element(el_start)?;
        let marker_text = "<__forge_move_marker__ />";
        let at = self.text.find(marker_text).context("move marker lost")?;
        self.splice(at..at + marker_text.len(), &source);
        Ok(())
    }

    /// Where an element's text starts after an insertion before it shifted it.
    fn find_moved(&self, source: &str, old_start: usize) -> Result<usize> {
        self.elements()?
            .into_iter()
            .filter(|e| e.range.start >= old_start && &self.text[e.range.clone()] == source)
            .map(|e| e.range.start)
            .next()
            .context("moved element lost")
    }

    /// Removes the element when it has no child elements left.
    pub fn remove_if_empty(&mut self, element: usize) -> Result<bool> {
        if self.children(element)?.is_empty() {
            self.remove_element(element)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROJECT: &str = "<Project Sdk=\"Microsoft.NET.Sdk\">\n\n  <!-- keep me -->\n  <ItemGroup>\n    <PackageReference Include=\"A\" Version=\"1.0.0\" />\n  </ItemGroup>\n\n</Project>\n";

    #[test]
    fn edits_keep_the_rest_of_the_file() {
        let mut xml = XmlText::new(PROJECT);
        let reference = xml.elements().unwrap().into_iter().find(|e| e.name == "PackageReference").unwrap();
        xml.set_attribute(reference.start(), "Version", "2.0.0").unwrap();
        assert!(xml.as_str().contains("<PackageReference Include=\"A\" Version=\"2.0.0\" />"));
        assert!(xml.as_str().contains("<!-- keep me -->"));

        let group = xml.elements().unwrap().into_iter().find(|e| e.name == "ItemGroup").unwrap();
        xml.append_child(group.start(), &NewElement::new("PackageReference").attr("Include", "B").attr("Version", "1.2")).unwrap();
        assert!(xml.as_str().contains("    <PackageReference Include=\"A\" Version=\"2.0.0\" />\n    <PackageReference Include=\"B\" Version=\"1.2\" />\n  </ItemGroup>"));

        let a = xml.elements().unwrap().into_iter().find(|e| e.attr("Include") == Some("A")).unwrap();
        xml.remove_element(a.start()).unwrap();
        assert_eq!(
            xml.as_str(),
            "<Project Sdk=\"Microsoft.NET.Sdk\">\n\n  <!-- keep me -->\n  <ItemGroup>\n    <PackageReference Include=\"B\" Version=\"1.2\" />\n  </ItemGroup>\n\n</Project>\n"
        );
    }

    #[test]
    fn appending_to_a_self_closing_element_and_crlf() {
        let mut xml = XmlText::new("<Solution>\r\n  <Folder Name=\"/src/\" />\r\n</Solution>\r\n");
        let folder = xml.elements().unwrap().into_iter().find(|e| e.name == "Folder").unwrap();
        xml.append_child(folder.start(), &NewElement::new("File").attr("Path", "a&b.md")).unwrap();
        assert_eq!(xml.as_str(), "<Solution>\r\n  <Folder Name=\"/src/\">\r\n    <File Path=\"a&amp;b.md\" />\r\n  </Folder>\r\n</Solution>\r\n");
    }

    #[test]
    fn moving_and_removing_attributes() {
        let mut xml = XmlText::new("<S>\n  <F Name=\"a\">\n    <P Path=\"x\" Type=\"t\" />\n  </F>\n  <F Name=\"b\" />\n</S>\n");
        let p = xml.elements().unwrap().into_iter().find(|e| e.name == "P").unwrap();
        xml.remove_attribute(p.start(), "Type").unwrap();
        let b = xml.elements().unwrap().into_iter().find(|e| e.attr("Name") == Some("b")).unwrap();
        let p = xml.elements().unwrap().into_iter().find(|e| e.name == "P").unwrap();
        xml.move_element(p.start(), b.start()).unwrap();
        assert_eq!(xml.as_str(), "<S>\n  <F Name=\"a\">\n  </F>\n  <F Name=\"b\">\n    <P Path=\"x\" />\n  </F>\n</S>\n");
    }

    #[test]
    fn set_text() {
        let mut xml = XmlText::new("<P><V>1</V><E/></P>");
        let v = xml.elements().unwrap().into_iter().find(|e| e.name == "V").unwrap();
        xml.set_text(v.start(), "2").unwrap();
        let e = xml.elements().unwrap().into_iter().find(|e| e.name == "E").unwrap();
        xml.set_text(e.start(), "x").unwrap();
        assert_eq!(xml.as_str(), "<P><V>2</V><E>x</E></P>");
    }
}

#[cfg(test)]
mod bom_tests {
    use super::*;

    #[test]
    fn files_with_a_byte_order_mark() {
        let mut xml = XmlText::new("\u{feff}<Project>\n  <ItemGroup />\n</Project>\n");
        let group = xml.elements().unwrap().into_iter().find(|e| e.name == "ItemGroup").unwrap();
        xml.append_child(group.start(), &NewElement::new("Compile").attr("Include", "A.cs")).unwrap();
        assert!(xml.as_str().starts_with('\u{feff}'));
        assert!(xml.as_str().contains("<ItemGroup>\n    <Compile Include=\"A.cs\" />\n  </ItemGroup>"), "{}", xml.as_str());
    }
}
