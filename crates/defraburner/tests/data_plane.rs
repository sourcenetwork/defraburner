//! End-to-end tenant data lifecycle (Milestone 2 addendum, D25): create a
//! tenant with a real schema, then create/read/update/read/delete a
//! document through the gateway's normal tenant-token GraphQL path --
//! exactly the path the dashboard's Data view drives, never an admin
//! backdoor. Verified wire shapes (from a direct read of defradb.rs's
//! live query-parse/query source, not the dead schema_gen CLI path):
//! `add_X`/`update_X`/`delete_X` all reply with a JSON *array*, even for
//! a single document; `add_X` takes a list `input`.

mod common;
use common::*;

const SDL: &str = "type Post { title: String views: Int }";

#[test]
fn a_document_survives_create_read_update_read_delete_confirm_gone() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data_root = tmp.path().join("data");
    let ready_file = tmp.path().join("ready.json");
    let mut child = spawn_up(
        &data_root,
        &ready_file,
        &["--no-open", "--gateway-addr", "127.0.0.1:0"],
    );
    wait_for_ready_file(&ready_file, &mut child, READY_DEADLINE);

    let admin_token = read_admin_token(&data_root);
    let base_url = read_gateway_base_url(&data_root);

    let created_tenant = create_tenant(&base_url, &admin_token, "acme-co", SDL, 1);
    let token = created_tenant["token"]
        .as_str()
        .expect("tenant create response should carry a token")
        .to_string();

    // ---- create: add_Post(input: [{...}]) -> data.add_Post is an array
    let create_query =
        r#"mutation { add_Post(input: [{title: "Hello", views: 0}]) { _docID title views } }"#;
    let create_response = graphql(&base_url, &token, create_query);
    assert!(
        create_response["errors"].is_null(),
        "create should not error: {create_response}"
    );
    let created = &create_response["data"]["add_Post"];
    assert!(
        created.is_array(),
        "add_Post should reply with an array, per the verified wire shape: {create_response}"
    );
    let created = &created[0];
    let doc_id = created["_docID"]
        .as_str()
        .expect("created doc should carry a _docID")
        .to_string();
    assert_eq!(created["title"], "Hello");
    assert_eq!(created["views"], 0);

    // ---- read: filtered query by docID (deadline-polled: a single-cell,
    // single-replica tenant has no cross-cell replication window here,
    // but polling costs nothing and keeps this test robust if that ever
    // changes).
    let read_back = wait_for_doc(&base_url, &token, &doc_id, "title", "Hello");
    assert_eq!(read_back["views"], 0);

    // ---- update: update_Post(docID, input) -> data.update_Post is an
    // array; docID is always explicit, never omitted (omitting it, or
    // filter, updates every document in the collection -- a real
    // footgun the UI and this test both avoid).
    let update_query = format!(
        r#"mutation {{ update_Post(docID: "{doc_id}", input: {{views: 42}}) {{ _docID title views }} }}"#
    );
    let update_response = graphql(&base_url, &token, &update_query);
    assert!(
        update_response["errors"].is_null(),
        "update should not error: {update_response}"
    );
    let updated = &update_response["data"]["update_Post"];
    assert!(
        updated.is_array(),
        "update_Post should reply with an array: {update_response}"
    );
    assert_eq!(updated[0]["_docID"], doc_id);
    assert_eq!(updated[0]["views"], 42);
    assert_eq!(
        updated[0]["title"], "Hello",
        "an update that only sets views must not clobber title"
    );

    // ---- read updated: confirm the change is genuinely visible on a
    // fresh query, not just echoed back by the mutation response.
    let read_updated = wait_for_doc(&base_url, &token, &doc_id, "views", 42);
    assert_eq!(read_updated["title"], "Hello");

    // ---- total count via COUNT: the honest, real count mechanism this
    // console's Data view uses for "showing X-Y of Z" (not the
    // addendum's page-only fallback, since COUNT is real and available).
    let count_query =
        format!(r#"query {{ n: COUNT(Post: {{filter: {{_docID: {{_eq: "{doc_id}"}}}}}}) }}"#);
    let count_response = graphql(&base_url, &token, &count_query);
    assert_eq!(
        count_response["data"]["n"], 1,
        "COUNT should see exactly the one document: {count_response}"
    );

    // ---- delete: delete_Post(docID) -> data.delete_Post is an array;
    // docID always explicit here too (omitting docID/filter deletes
    // every document in the collection).
    let delete_query = format!(r#"mutation {{ delete_Post(docID: "{doc_id}") {{ _docID }} }}"#);
    let delete_response = graphql(&base_url, &token, &delete_query);
    assert!(
        delete_response["errors"].is_null(),
        "delete should not error: {delete_response}"
    );
    let deleted = &delete_response["data"]["delete_Post"];
    assert!(
        deleted.is_array(),
        "delete_Post should reply with an array: {delete_response}"
    );
    assert_eq!(deleted[0]["_docID"], doc_id);

    // ---- confirm gone: both a direct query and COUNT agree the
    // document is genuinely gone, not just marked in the mutation reply.
    wait_until(CONDITION_DEADLINE, STATUS_POLL_STEP, || {
        let query =
            format!(r#"query {{ Post(filter: {{_docID: {{_eq: "{doc_id}"}}}}) {{ _docID }} }}"#);
        let response = graphql(&base_url, &token, &query);
        response["data"]["Post"]
            .as_array()
            .map(|rows| rows.is_empty())
            .unwrap_or(false)
    });
    let final_count = graphql(
        &base_url,
        &token,
        &format!(r#"query {{ n: COUNT(Post: {{filter: {{_docID: {{_eq: "{doc_id}"}}}}}}) }}"#),
    );
    assert_eq!(
        final_count["data"]["n"], 0,
        "COUNT should confirm the document is gone: {final_count}"
    );

    send_sigterm(&child);
    wait_for_exit(&mut child, EXIT_DEADLINE);
}

/// Every data mutation goes through the gateway's normal tenant-token
/// path, so admission applies exactly like any other request -- proven
/// here by driving the same `Post` collection with the admin-free tenant
/// token, never a backdoor.
#[test]
fn schema_discovery_via_introspection_finds_the_real_collection_and_fields() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data_root = tmp.path().join("data");
    let ready_file = tmp.path().join("ready.json");
    let mut child = spawn_up(
        &data_root,
        &ready_file,
        &["--no-open", "--gateway-addr", "127.0.0.1:0"],
    );
    wait_for_ready_file(&ready_file, &mut child, READY_DEADLINE);

    let admin_token = read_admin_token(&data_root);
    let base_url = read_gateway_base_url(&data_root);
    let created_tenant = create_tenant(&base_url, &admin_token, "acme-co", SDL, 1);
    let token = created_tenant["token"].as_str().unwrap().to_string();

    // The Data view's own discovery query: a real collection's list field
    // carries filter/limit/offset args; distinguishes it from COUNT/SUM/
    // introspection meta-fields without any admin access.
    let discovery = graphql(
        &base_url,
        &token,
        "{ __type(name: \"Query\") { fields { name args { name } } } }",
    );
    let fields = discovery["data"]["__type"]["fields"]
        .as_array()
        .expect("Query fields");
    let post_field = fields
        .iter()
        .find(|f| f["name"] == "Post")
        .expect("Post should be discoverable via introspection");
    let arg_names: Vec<&str> = post_field["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert!(arg_names.contains(&"filter"));
    assert!(arg_names.contains(&"limit"));
    assert!(arg_names.contains(&"offset"));

    let field_discovery = graphql(
        &base_url,
        &token,
        "{ __type(name: \"Post\") { fields { name type { kind name } } } }",
    );
    let post_fields = field_discovery["data"]["__type"]["fields"]
        .as_array()
        .expect("Post fields");
    let names: Vec<&str> = post_fields
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"title"));
    assert!(names.contains(&"views"));

    send_sigterm(&child);
    wait_for_exit(&mut child, EXIT_DEADLINE);
}

fn graphql(base_url: &str, token: &str, query: &str) -> serde_json::Value {
    ureq::post(&format!("{base_url}/api/v0/graphql"))
        .set("Authorization", &format!("Bearer {token}"))
        .send_json(serde_json::json!({ "query": query }))
        .unwrap_or_else(|error| {
            panic!("graphql request should succeed at the transport level: {error}")
        })
        .into_json()
        .expect("valid JSON response")
}

/// Deadline-polls a filtered `Post` query by `doc_id` until `field`
/// equals `expected`, returning the matched document. Never a bare
/// sleep: fast when already consistent, still bounded if it is not.
fn wait_for_doc(
    base_url: &str,
    token: &str,
    doc_id: &str,
    field: &str,
    expected: impl Into<serde_json::Value>,
) -> serde_json::Value {
    let expected = expected.into();
    let query = format!(
        r#"query {{ Post(filter: {{_docID: {{_eq: "{doc_id}"}}}}) {{ _docID title views }} }}"#
    );
    wait_until(CONDITION_DEADLINE, STATUS_POLL_STEP, || {
        let response = graphql(base_url, token, &query);
        let rows = response["data"]["Post"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        rows.first()
            .map(|row| row[field] == expected)
            .unwrap_or(false)
    });
    let response = graphql(base_url, token, &query);
    response["data"]["Post"][0].clone()
}
