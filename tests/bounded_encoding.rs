#![cfg(feature = "alloc")]

use ubus::{
    blobmsg::{BlobBuilder, BlobType},
    bounded::{BlobSliceBuilder, EnvSliceBuilder},
    envelope::{EnvBuilder, UbusAttr},
    wire::{Codec, FrameRef, Header, MsgType},
    BufferError,
};

fn header() -> Header {
    Header {
        version: 0,
        ty: MsgType::Notify,
        seq: 77,
        peer: 9,
    }
}

#[test]
fn fixed_builders_match_owned_wire_at_every_capacity_boundary() {
    let mut expected = EnvBuilder::new();
    expected.push_u32(UbusAttr::ObjId, 42);
    expected.push_str(UbusAttr::Method, "signal");
    expected.push_u8(UbusAttr::NoReply, 1);
    let expected = expected.finish();
    for capacity in 0..=expected.len() {
        let mut storage = vec![0xaa; capacity];
        let result = (|| {
            let mut builder = EnvSliceBuilder::new(&mut storage)?;
            builder.push_u32(UbusAttr::ObjId, 42)?;
            builder.push_str(UbusAttr::Method, "signal")?;
            builder.push_u8(UbusAttr::NoReply, 1)?;
            Ok::<_, BufferError>(builder.finish().len())
        })();
        if capacity == expected.len() {
            let n = result.unwrap();
            assert_eq!(&storage[..n], expected);
        } else {
            assert!(matches!(result, Err(BufferError::Capacity { .. })));
        }
    }
    let frame = FrameRef {
        hdr: header(),
        payload: &expected,
    };
    let owned = Codec::write_frame(&frame.to_owned());
    for capacity in 0..=owned.len() {
        let mut output = vec![0xaa; capacity];
        match Codec::write_frame_to_slice(frame, &mut output) {
            Ok(n) => {
                assert_eq!(capacity, owned.len());
                assert_eq!(&output[..n], owned);
                let (decoded, consumed) = Codec::read_frame_borrowed(&output).unwrap();
                assert_eq!(decoded, frame);
                assert_eq!(consumed, n);
            }
            Err(BufferError::Capacity { .. }) => assert!(output.iter().all(|v| *v == 0xaa)),
            result => panic!("unexpected result: {result:?}"),
        }
    }
}

#[test]
fn fixed_blobmsg_matches_owned_nested_types_and_anonymous_names() {
    let mut owned = BlobBuilder::new();
    owned.open_table(None);
    owned.put_str(Some("message"), "interrupt");
    owned.put_i32(Some("signal"), 10);
    owned.put_i64(Some("counter"), i64::MIN);
    owned.put_bool(Some("ready"), true);
    owned.put_double(Some("value"), 2.5);
    owned.open_array(Some("values"));
    owned.put_str(None, "one");
    owned.put_i32(None, -2);
    owned.close_array();
    owned.close_table();
    let expected = owned.finish();
    let mut storage = [0_u8; 512];
    let mut fixed = BlobSliceBuilder::<4>::new(&mut storage);
    fixed.open_table(None).unwrap();
    fixed.put_str(Some("message"), "interrupt").unwrap();
    fixed.put_i32(Some("signal"), 10).unwrap();
    fixed.put_i64(Some("counter"), i64::MIN).unwrap();
    fixed.put_bool(Some("ready"), true).unwrap();
    fixed.put_double(Some("value"), 2.5).unwrap();
    fixed.open_array(Some("values")).unwrap();
    fixed.put_str(None, "one").unwrap();
    fixed.put_i32(None, -2).unwrap();
    fixed.close().unwrap();
    fixed.close().unwrap();
    assert_eq!(fixed.finish().unwrap(), expected);
}

#[test]
fn rejected_appends_preserve_valid_data_and_container_state() {
    let mut storage = [0_u8; 24];
    let mut fixed = BlobSliceBuilder::<1>::new(&mut storage);
    assert_eq!(fixed.close(), Err(BufferError::Unbalanced));
    fixed.open_table(None).unwrap();
    assert_eq!(fixed.open_array(None), Err(BufferError::NestingLimit));
    assert_eq!(
        fixed.put_str(Some("bad\0key"), "x"),
        Err(BufferError::EmbeddedNul)
    );
    assert_eq!(
        fixed.put_str(None, "bad\0value"),
        Err(BufferError::EmbeddedNul)
    );
    assert!(matches!(
        fixed.put_str(None, "far too large for the buffer"),
        Err(BufferError::Capacity { .. })
    ));
    fixed.put_i32(None, 7).unwrap();
    fixed.close().unwrap();
    let bytes = fixed.finish().unwrap();
    let outer = ubus::BlobIter::new(bytes).next().unwrap();
    assert_eq!(outer.ty, BlobType::Table);
    assert_eq!(
        ubus::BlobIter::new(outer.payload).next().unwrap().as_i32(),
        Some(7)
    );

    let mut storage = [0; 12];
    let mut env = EnvSliceBuilder::new(&mut storage).unwrap();
    assert_eq!(env.push_bytes_id(128, &[]), Err(BufferError::InvalidId));
    env.push_u32(UbusAttr::ObjId, 9).unwrap();
    assert!(matches!(
        env.push_u8(UbusAttr::NoReply, 1),
        Err(BufferError::Capacity { .. })
    ));
    assert_eq!(
        ubus::envelope::find(env.finish(), UbusAttr::ObjId)
            .unwrap()
            .as_u32(),
        Some(9)
    );
}

#[test]
fn unclosed_or_malformed_output_is_rejected_without_touching_destination() {
    let mut storage = [0; 16];
    let mut builder = BlobSliceBuilder::<1>::new(&mut storage);
    builder.open_table(None).unwrap();
    assert_eq!(builder.finish(), Err(BufferError::Unbalanced));
    for payload in [&[0_u8][..], &[0, 0, 0, 3][..], &[0, 0, 0, 8][..]] {
        let mut output = [0xaa; 32];
        assert_eq!(
            Codec::write_frame_to_slice(
                FrameRef {
                    hdr: header(),
                    payload
                },
                &mut output
            ),
            Err(BufferError::MalformedFrame)
        );
        assert_eq!(output, [0xaa; 32]);
    }
    let mut output = [0xaa; 16];
    let n = Codec::write_frame_to_slice(
        FrameRef {
            hdr: header(),
            payload: &[0, 0, 0, 5, 7],
        },
        &mut output,
    )
    .unwrap();
    assert_eq!(n, 13);
    assert_eq!(&output[n..], &[0xaa; 3]);
    assert_eq!(
        Codec::read_frame_borrowed(&output[..n]).unwrap().0.payload,
        &[0, 0, 0, 5, 7]
    );
}
