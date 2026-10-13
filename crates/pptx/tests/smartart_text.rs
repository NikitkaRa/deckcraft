//! SmartArt text is laid out in the drawing's own text rectangle (`dsp:txXfrm`), which turns back
//! the shape's rotation (#231, #232).

use std::io::{Cursor, Read, Write};

use deckcraft_engine::Session;
use deckcraft_model::{Shape, ShapeKind};
use zip::write::SimpleFileOptions;

const A: &str = r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main""#;

fn drawing() -> String {
    format!(
        r#"<?xml version="1.0"?><dsp:drawing xmlns:dsp="http://schemas.microsoft.com/office/drawing/2008/diagram" {A}><dsp:spTree><dsp:nvGrpSpPr><dsp:cNvPr id="0" name=""/><dsp:cNvGrpSpPr/></dsp:nvGrpSpPr><dsp:grpSpPr/>
<dsp:sp modelId="{{1}}"><dsp:nvSpPr><dsp:cNvPr id="0" name=""/><dsp:cNvSpPr/></dsp:nvSpPr><dsp:spPr><a:xfrm rot="10800000"><a:off x="0" y="0"/><a:ext cx="1270000" cy="635000"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom><a:solidFill><a:srgbClr val="4472C4"/></a:solidFill></dsp:spPr><dsp:txBody><a:bodyPr/><a:p><a:r><a:t>Upright</a:t></a:r></a:p></dsp:txBody><dsp:txXfrm rot="10800000"><a:off x="0" y="0"/><a:ext cx="1270000" cy="635000"/></dsp:txXfrm></dsp:sp>
<dsp:sp modelId="{{2}}"><dsp:nvSpPr><dsp:cNvPr id="0" name=""/><dsp:cNvSpPr/></dsp:nvSpPr><dsp:spPr><a:xfrm rot="5400000"><a:off x="1905000" y="-635000"/><a:ext cx="635000" cy="1905000"/></a:xfrm><a:prstGeom prst="round2SameRect"><a:avLst/></a:prstGeom></dsp:spPr><dsp:txBody><a:bodyPr/><a:p><a:r><a:t>Is there any concern?</a:t></a:r></a:p></dsp:txBody><dsp:txXfrm rot="-5400000"><a:off x="1270000" y="31000"/><a:ext cx="1874000" cy="573000"/></dsp:txXfrm></dsp:sp>
</dsp:spTree></dsp:drawing>"#
    )
}

const DATA: &str = r#"<?xml version="1.0"?><dgm:dataModel xmlns:dgm="http://schemas.openxmlformats.org/drawingml/2006/diagram" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:dsp="http://schemas.microsoft.com/office/drawing/2008/diagram"><dgm:ptLst/><dgm:extLst><a:ext uri="http://schemas.microsoft.com/office/drawing/2008/diagram"><dsp:dataModelExt relId="rIdDsp"/></a:ext></dgm:extLst></dgm:dataModel>"#;

fn frame() -> String {
    format!(
        r#"<p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="900" name="Diagram 1"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr><p:xfrm><a:off {A} x="635000" y="635000"/><a:ext {A} cx="3175000" cy="635000"/></p:xfrm><a:graphic {A}><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/diagram"><dgm:relIds xmlns:dgm="http://schemas.openxmlformats.org/drawingml/2006/diagram" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" r:dm="rIdDm" r:lo="" r:qs="" r:cs=""/></a:graphicData></a:graphic></p:graphicFrame>"#
    )
}

const RELS: &str = r#"<Relationship Id="rIdDm" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/diagramData" Target="../diagrams/data1.xml"/><Relationship Id="rIdDsp" Type="http://schemas.microsoft.com/office/2007/relationships/diagramDrawing" Target="../diagrams/drawing1.xml"/>"#;

fn deck() -> Vec<u8> {
    let mut s = Session::new();
    s.execute("file.new", &serde_json::json!({"blank": true})).expect("new");
    s.execute("slide.new", &serde_json::json!({"layout": "blank"})).expect("slide");
    let p = s.doc().expect("doc").doc.as_ref().clone();
    let bytes = deckcraft_pptx::export(&p).expect("export");
    let mut src = zip::ZipArchive::new(Cursor::new(bytes)).expect("zip");
    let mut out = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let names: Vec<String> = (0..src.len()).map(|i| src.by_index(i).expect("entry").name().to_string()).collect();
    let mut had_rels = false;
    for name in &names {
        let mut data = String::new();
        src.by_name(name).expect("by_name").read_to_string(&mut data).ok();
        let mut raw = Vec::new();
        if data.is_empty() {
            src.by_name(name).expect("by_name").read_to_end(&mut raw).expect("read");
        }
        if name == "ppt/slides/slide1.xml" {
            data = data.replacen("</p:spTree>", &format!("{}</p:spTree>", frame()), 1);
        } else if name == "ppt/slides/_rels/slide1.xml.rels" {
            had_rels = true;
            data = data.replacen("</Relationships>", &format!("{RELS}</Relationships>"), 1);
        }
        out.start_file(name, SimpleFileOptions::default()).expect("start");
        out.write_all(if raw.is_empty() { data.as_bytes() } else { &raw }).expect("write");
    }
    assert!(had_rels, "test setup: slide1 has rels");
    for (name, data) in [("ppt/diagrams/data1.xml", DATA.to_string()), ("ppt/diagrams/drawing1.xml", drawing())] {
        out.start_file(name, SimpleFileOptions::default()).expect("start");
        out.write_all(data.as_bytes()).expect("write");
    }
    out.finish().expect("finish").into_inner()
}

fn texts<'a>(shapes: &'a [Shape], out: &mut Vec<&'a Shape>) {
    for s in shapes {
        if let ShapeKind::Group { children, .. } = &s.kind {
            texts(children, out);
        } else if s.text.as_ref().is_some_and(|t| !t.text().is_empty()) {
            out.push(s);
        }
    }
}

#[test]
fn smartart_text_uses_its_own_rectangle_and_rotation() {
    let p = deckcraft_pptx::import(&deck()).expect("import");
    let mut found = vec![];
    texts(&p.slides[0].shapes, &mut found);
    let get = |t: &str| found.iter().find(|s| s.text.as_ref().is_some_and(|b| b.text() == t)).copied().expect(t);

    // 180° shape with a 180° text rectangle: the text is upright.
    let x = get("Upright").xfrm.expect("xfrm");
    assert!(x.rot.rem_euclid(360.0) < 0.01, "text box rot {}", x.rot);

    // 90° shape with a −90° text rectangle: the text is horizontal and as wide as txXfrm.
    let wide = get("Is there any concern?");
    let x = wide.xfrm.expect("xfrm");
    assert!(x.rot.rem_euclid(360.0) < 0.01, "text box rot {}", x.rot);
    assert!(x.w > x.h * 3.0, "text box {} x {}", x.w, x.h);

    // Each shape keeps its geometry and fill, without the text.
    let all = match &p.slides[0].shapes.iter().find(|s| matches!(s.kind, ShapeKind::Group { .. })).expect("group").kind {
        ShapeKind::Group { children, .. } => children.clone(),
        _ => vec![],
    };
    assert_eq!(all.len(), 4);
}
