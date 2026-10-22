use ssh_transport::SshDestination;
#[test]
fn accepts_only_destination_forms_without_option_or_command_injection() {
    for (input, host, user, port) in [
        ("production", "production", None, None),
        ("deploy@example.test", "example.test", Some("deploy"), None),
        (
            "ssh deploy@example.test",
            "example.test",
            Some("deploy"),
            None,
        ),
        (
            "ssh://deploy@example.test:2200",
            "example.test",
            Some("deploy"),
            Some(2200),
        ),
        (
            "ssh://deploy@[2001:db8::1]:2200",
            "2001:db8::1",
            Some("deploy"),
            Some(2200),
        ),
        ("deploy@[2001:db8::1]", "2001:db8::1", Some("deploy"), None),
    ] {
        assert_eq!(
            SshDestination::parse(input).unwrap(),
            SshDestination {
                host: host.into(),
                user: user.map(str::to_owned),
                port
            },
            "{input}"
        );
    }
    for bad in [
        "-oProxyCommand=touch /tmp/pwn",
        "ssh -p 22 host",
        "host command",
        "user@host;id",
        "ssh://user:password@host",
        "ssh://host/path",
        "host\nid",
        "ssh://host:0",
        "ssh://host:99999",
        "user@@host",
        "$(id)",
        "ssh://[::1",
        "ssh://host?key=x",
    ] {
        assert!(SshDestination::parse(bad).is_err(), "accepted {bad}");
    }
}
