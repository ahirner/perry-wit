use perry_time_helper::{CalendarError, Error, Instant, PlainDateTime, shift_iso_date};

#[test]
fn standalone_calendar_contract_rejects_normalization_and_offset_overflow() {
    for (input, days, expected) in [
        ("2024-02-28", 1, "2024-02-29"),
        ("2024-02-29", 1, "2024-03-01"),
        ("1900-02-28", 1, "1900-03-01"),
        ("2000-03-01", -1, "2000-02-29"),
        ("2024-01-01", -1, "2023-12-31"),
        ("0001-01-01", 3_652_058, "9999-12-31"),
        ("9999-12-31", -3_652_058, "0001-01-01"),
    ] {
        assert_eq!(
            shift_iso_date(input.as_bytes(), days).unwrap(),
            expected.as_bytes()
        );
    }
    for input in [
        "2023-02-29",
        "1900-02-29",
        "2024-02-30",
        "0000-01-01",
        "2024-00-01",
        "2024-13-01",
        "2024-01-00",
        "2024-01-32",
        "2024-2-01",
        "2024/01/01",
        "20240101",
        " 2024-01-01",
        "2024-01-01Z",
        "é024-01-01",
    ] {
        assert_eq!(
            shift_iso_date(input.as_bytes(), 0),
            Err(CalendarError::InvalidDate),
            "{input}"
        );
    }
    for (input, days) in [
        ("0001-01-01", -1),
        ("9999-12-31", 1),
        ("2024-01-01", i32::MAX),
        ("2024-01-01", i32::MIN),
    ] {
        assert_eq!(
            shift_iso_date(input.as_bytes(), days),
            Err(CalendarError::OutOfRange)
        );
    }
}

#[test]
fn strict_utc_interchange_keeps_exact_fractions_and_rejects_other_forms() {
    for (input, nanoseconds, canonical) in [
        ("1970-01-01T00:00:00Z", 0, "1970-01-01T00:00:00Z"),
        (
            "1970-01-01T00:00:00.000000001Z",
            1,
            "1970-01-01T00:00:00.000000001Z",
        ),
        (
            "1969-12-31T23:59:59.999999999Z",
            -1,
            "1969-12-31T23:59:59.999999999Z",
        ),
        (
            "1970-01-01T00:00:00.123400000Z",
            123_400_000,
            "1970-01-01T00:00:00.1234Z",
        ),
    ] {
        let value = Instant::parse_utc(input.as_bytes()).unwrap();
        assert_eq!(value.epoch_nanoseconds(), nanoseconds);
        let mut output = [0; 33];
        let length = value.format(&mut output).unwrap();
        assert_eq!(&output[..length], canonical.as_bytes());
        assert_eq!(Instant::parse_utc(&output[..length]).unwrap(), value);
    }
    for input in [
        "2024-02-30T00:00:00Z",
        "2023-02-29T00:00:00Z",
        "2024-01-01",
        "2024-01-01T00:00Z",
        "2024-01-01 00:00:00Z",
        "2024-01-01t00:00:00z",
        "2024-01-01T00:00:00",
        "2024-01-01T00:00:00+00:00",
        "2024-01-01T00:00:00-01:00",
        "2024-01-01T00:00:00.Z",
        "2024-01-01T00:00:00.1234567890Z",
        "2024-01-01T00:00:00,1Z",
        "2024-01-01T24:00:00Z",
        "2024-01-01T00:60:00Z",
        "2024-01-01T00:00:60Z",
        "2024-01-01T00:00:00Z[UTC]",
        "+002024-01-01T00:00:00Z",
        "2024-01-01T00:00:00Z\n",
    ] {
        assert!(Instant::parse_utc(input.as_bytes()).is_err(), "{input}");
    }
    assert!(Instant::parse_utc(&[0xff; 20]).is_err());
}

#[test]
fn instant_offsets_epoch_floor_and_limits_follow_temporal() {
    for (input, expected) in [
        ("1970-01-01T01:00+01:00", 0),
        ("1969-12-31T23:00-01:00", 0),
        ("1970-01-01T00:00:00-00:00", 0),
        ("1970-01-01T00:00:00.123456789+00:00:00.123456788", 1),
        ("19700101T000000Z", 0),
        ("1970-01-01T00:00:60Z", 59_000_000_000),
    ] {
        assert_eq!(
            Instant::parse(input.as_bytes())
                .unwrap()
                .epoch_nanoseconds(),
            expected
        );
    }
    for (nanoseconds, milliseconds) in [(-1, -1), (-1_000_001, -2), (999_999, 0), (1_000_001, 1)] {
        assert_eq!(
            Instant::from_epoch_nanoseconds(nanoseconds)
                .unwrap()
                .epoch_milliseconds(),
            milliseconds
        );
    }
    let limit = 8_640_000_000_000_000_000_000i128;
    for (nanoseconds, expected) in [
        (-limit, "-271821-04-20T00:00:00Z"),
        (limit, "+275760-09-13T00:00:00Z"),
    ] {
        let value = Instant::from_epoch_nanoseconds(nanoseconds).unwrap();
        let mut output = [0; 33];
        let length = value.format(&mut output).unwrap();
        assert_eq!(&output[..length], expected.as_bytes());
        assert_eq!(Instant::parse(&output[..length]).unwrap(), value);
    }
    for nanoseconds in [-limit - 1, limit + 1, i128::MIN, i128::MAX] {
        assert_eq!(
            Instant::from_epoch_nanoseconds(nanoseconds),
            Err(Error::Range)
        );
    }
    for milliseconds in [
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        0.1,
        -0.1,
        8_640_000_000_000_001.0,
    ] {
        assert_eq!(
            Instant::from_epoch_milliseconds(milliseconds),
            Err(Error::Range)
        );
    }
    assert_eq!(
        Instant::from_epoch_milliseconds(-0.0)
            .unwrap()
            .epoch_nanoseconds(),
        0
    );
    for input in [
        "2024-01-01",
        "2024-01-01T00:00",
        "2024-02-30T00:00Z",
        "2024-01-01T00:00+24:00",
        "2024-01-01T00:00:00.1234567890Z",
        "2024-01-01T00:00:00+00:00:00.1234567890",
        "-000000-01-01T00:00Z",
    ] {
        assert!(Instant::parse(input.as_bytes()).is_err(), "{input}");
    }
}

#[test]
fn plain_calendar_days_keep_time_fields_and_have_no_time_zone() {
    let date = PlainDateTime::parse(b"2024-02-28T23:59:59.123456789").unwrap();
    let next = date.add_days(1).unwrap();
    assert_eq!(next.date(), (2024, 2, 29));
    assert_eq!(next.time(), (23, 59, 59, 123_456_789));
    assert_eq!(next.add_days(-1).unwrap(), date);
    assert_eq!(
        PlainDateTime::parse(b"2024-02-28T23:59:59.123456789+09:00").unwrap(),
        date
    );
    assert_eq!(
        PlainDateTime::parse(b"2024-02-29").unwrap().time(),
        (0, 0, 0, 0)
    );
    assert!(PlainDateTime::parse(b"2024-02-28T23:59:59Z").is_err());
    assert!(date.add_days(i64::MAX).is_err());
    assert!(date.add_days(i64::MIN).is_err());
    for input in [
        "-271821-04-19T00:00:00.000000001",
        "+275760-09-13T23:59:59.999999999",
    ] {
        let value = PlainDateTime::parse(input.as_bytes()).unwrap();
        let mut output = [0; 33];
        let length = value.format(&mut output).unwrap();
        assert_eq!(&output[..length], input.as_bytes());
    }
    for input in [
        "-271821-04-19T00:00:00",
        "+275760-09-14T00:00:00",
        "2024-02-30",
        "2024-01-01T00:00:00.1234567890",
    ] {
        assert!(PlainDateTime::parse(input.as_bytes()).is_err(), "{input}");
    }
}

#[test]
fn excluded_annotations_and_short_destinations_fail_explicitly_without_writes() {
    for input in [
        "2024-01-01T00:00:00Z[UTC]",
        "2024-01-01T00:00:00+00:00[u-ca=iso8601]",
    ] {
        assert_eq!(
            Instant::parse(input.as_bytes()),
            Err(Error::UnsupportedAnnotation)
        );
        assert_eq!(
            PlainDateTime::parse(input.as_bytes()),
            Err(Error::UnsupportedAnnotation)
        );
    }
    let instant = Instant::parse(b"2000-02-29T12:34:56.123456789Z").unwrap();
    let plain = PlainDateTime::parse(b"2000-02-29T12:34:56.123456789").unwrap();
    for length in 0..29 {
        let mut output = [0xa5; 33];
        assert_eq!(instant.format(&mut output[..length]), Err(Error::Capacity));
        assert_eq!(plain.format(&mut output[..length]), Err(Error::Capacity));
        assert_eq!(output, [0xa5; 33]);
    }
}
