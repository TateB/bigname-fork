// A label spelled `[<64 lowercase hex labelhash>]` addresses the node that labelhash names on
// every name-shaped input, in the registry-children fixture of v2_family_registry_children.rs.

fn bracketed(label: &str) -> String {
    format!("%5B{}%5D", child_labelhash(label).trim_start_matches("0x"))
}

#[tokio::test]
async fn v2_bracketed_labelhash_addresses_a_named_node_on_name_routes() -> Result<()> {
    let database = TestDatabase::new_migrated().await?;
    seed_registry_children_fixture(&database).await?;
    let gains_node = bigname_lookup::ens_namehash_hex("gains.alpha.eth")?;

    for spelling in [
        format!("{}.alpha.eth", bracketed("gains")),
        format!("gains.{}.eth", bracketed("alpha")),
        format!("{}.{}.eth", bracketed("gains"), bracketed("alpha")),
    ] {
        for suffix in ["", "/records"] {
            let uri = format!("/v1/names/{spelling}{suffix}");
            let (status, body) = read_family_response(&database, &uri).await?;
            assert_eq!(status, StatusCode::OK, "{uri}: {body:#}");
            let (_, plain) =
                read_family_response(&database, &format!("/v1/names/gains.alpha.eth{suffix}"))
                    .await?;
            assert_eq!(body["data"], plain["data"], "{uri}");
        }
        let uri = format!("/v1/names/{spelling}");
        let (_, body) = read_family_response(&database, &uri).await?;
        assert_eq!(body["data"]["name"], json!("gains.alpha.eth"), "{uri}: {body:#}");
        assert_eq!(body["data"]["display_name"], json!("gains.alpha.eth"), "{uri}: {body:#}");
        assert_eq!(body["data"]["namehash"], json!(gains_node), "{uri}: {body:#}");
        let uri = format!("/v1/names/{spelling}/history?page_size=50");
        let (status, body) = read_family_response(&database, &uri).await?;
        assert_eq!(status, StatusCode::OK, "{uri}: {body:#}");
        let (_, plain) =
            read_family_response(&database, "/v1/names/gains.alpha.eth/history?page_size=50")
                .await?;
        assert_eq!(body["data"], plain["data"], "{uri}");
    }

    // Name filters read the same node.
    for route in ["/v1/events?name=", "/v1/permissions?name="] {
        let uri = format!("{route}{}.eth", bracketed("alpha"));
        let (status, body) = read_family_response(&database, &uri).await?;
        assert_eq!(status, StatusCode::OK, "{uri}: {body:#}");
        let (_, plain) = read_family_response(&database, &format!("{route}alpha.eth")).await?;
        assert_eq!(body["data"], plain["data"], "{uri}");
        assert!(
            !body["data"].as_array().expect("rows").is_empty(),
            "{uri}: {body:#}"
        );
    }

    // The parent addressed by its labelhash lists the same children.
    let uri = format!("/v1/names/{}.eth/subnames?page_size=10", bracketed("alpha"));
    let (status, body) = read_family_response(&database, &uri).await?;
    assert_eq!(status, StatusCode::OK, "{uri}: {body:#}");
    let (_, plain) =
        read_family_response(&database, "/v1/names/alpha.eth/subnames?page_size=10").await?;
    assert_eq!(body["data"], plain["data"], "{uri}");

    database.cleanup().await
}

#[tokio::test]
async fn v2_bracketed_labelhash_of_a_node_with_no_name_row_is_not_found() -> Result<()> {
    let database = TestDatabase::new_migrated().await?;
    seed_registry_children_fixture(&database).await?;

    // `unknown` is a registry child with no name surface: its parent's subnames route serves it
    // by this spelling, but no name row composes for it.
    for suffix in ["", "/records", "/history", "/subnames"] {
        let uri = format!("/v1/names/{}.alpha.eth{suffix}", bracketed("unknown"));
        let (status, body) = read_family_response(&database, &uri).await?;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {body:#}");
        assert_eq!(body["error"]["code"], json!("not_found"), "{uri}: {body:#}");
    }

    database.cleanup().await
}

#[tokio::test]
async fn v2_bracketed_labelhash_must_be_64_lowercase_hex_digits() -> Result<()> {
    let database = TestDatabase::new_migrated().await?;
    seed_registry_children_fixture(&database).await?;
    let hex = child_labelhash("gains").trim_start_matches("0x").to_owned();

    for label in [
        format!("%5B{}%5D", hex.to_ascii_uppercase()),
        format!("%5B{}%5D", &hex[..63]),
        format!("%5B0x{}%5D", &hex[..62]),
        format!("%5B{hex}%5Dx"),
    ] {
        for suffix in ["", "/records", "/history", "/subnames"] {
            let uri = format!("/v1/names/{label}.alpha.eth{suffix}");
            let (status, body) = read_family_response(&database, &uri).await?;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body:#}");
            assert_eq!(body["error"]["code"], json!("invalid_input"), "{uri}: {body:#}");
        }
    }

    database.cleanup().await
}

#[tokio::test]
async fn v2_bracketed_labelhash_lookup_reads_the_node() -> Result<()> {
    let database = TestDatabase::new_migrated().await?;
    seed_registry_children_fixture(&database).await?;
    let hex = |label: &str| child_labelhash(label).trim_start_matches("0x").to_owned();
    let lookup = |name: String| {
        json!({"inputs": [{"id": "n", "name": name}], "profile": "detail"})
    };

    let response = v2_lookup_response_for_database_with_public_namespaces(
        &database,
        "/v1/lookup",
        lookup(format!("[{}].alpha.eth", hex("gains"))),
        &["ens"],
    )
    .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let bracketed: Value = read_json(response).await?;
    let response = v2_lookup_response_for_database_with_public_namespaces(
        &database,
        "/v1/lookup",
        lookup("gains.alpha.eth".to_owned()),
        &["ens"],
    )
    .await?;
    let plain: Value = read_json(response).await?;
    let record = &bracketed["data"][0]["record"];
    assert_eq!(record["name"], json!("gains.alpha.eth"), "{bracketed:#}");
    assert_eq!(record, &plain["data"][0]["record"], "{bracketed:#}");
    assert!(bracketed["data"][0].get("normalization").is_none(), "{bracketed:#}");

    // A malformed bracket is an in-band invalid name, as any other unnormalizable input.
    let response = v2_lookup_response_for_database_with_public_namespaces(
        &database,
        "/v1/lookup",
        lookup(format!("[{}].alpha.eth", hex("gains").to_ascii_uppercase())),
        &["ens"],
    )
    .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let invalid: Value = read_json(response).await?;
    assert_eq!(
        invalid["data"][0]["normalization"]["reason"],
        json!("invalid_normalized_name"),
        "{invalid:#}"
    );

    database.cleanup().await
}
