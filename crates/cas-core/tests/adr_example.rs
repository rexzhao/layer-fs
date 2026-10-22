//! Compiles and runs the example printed in
//! `docs/adr/ADR-001-object-encoding.md` §9, so the documentation cannot drift
//! away from the public API without a failing test.
//!
//! The body below is the ADR's `rust` block verbatim, including its own `use`.

#[test]
fn adr_section_9_example_compiles_and_passes() {
    use cas_core::{verify_id, Object, ObjectId, ObjectIdHasher, Role};

    // 1) 构造对象并取身份
    let object = Object::new(Role::Bytes, b"hello, world").unwrap();
    let id = object.id().unwrap();

    // 2) 规范帧 = 该对象的全部字节；写盘/哈希都用它
    let frame: Vec<u8> = object.to_vec();
    assert_eq!(&frame[0..4], b"WCAS");

    // 3) 解码：任何畸形字节都会被精确拒绝
    let decoded = Object::decode(&frame).unwrap();
    assert_eq!(decoded.value(), b"hello, world");
    assert_eq!(decoded.role(), Role::Bytes);

    // 4) 读路径校验：身份不符即报损坏
    verify_id(id, &frame).unwrap();

    // 5) 流式身份：分片喂入，不必驻留整对象
    let mut stream = ObjectIdHasher::start();
    stream.update_envelope(Role::Bytes, 12).unwrap();
    for chunk in b"hello, world".chunks(5) {
        stream.update_value(chunk);
    }
    assert_eq!(stream.finish(), id);

    // 6) 十六进制往返（64 个小写字符）
    let text = id.to_string();
    assert_eq!(text.len(), 64);
    assert_eq!(text.parse::<ObjectId>().unwrap(), id);
}
