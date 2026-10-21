use anyhow::{ensure, Result};
use hickory_proto::{
    op::{Message, MessageType, OpCode, Query, ResponseCode},
    rr::{DNSClass, Name, RecordType},
};

pub fn query(name: &str, kind: RecordType) -> Result<Message> {
    let mut message = Message::query();
    message.metadata.id =
        u16::from_ne_bytes(uuid::Uuid::new_v4().as_bytes()[..2].try_into().unwrap());
    message.metadata.recursion_desired = true;
    let mut name = Name::from_ascii(name)?;
    name.set_fqdn(true);
    message.add_query(Query::query(name, kind));
    Ok(message)
}
pub fn validate_query(message: &Message) -> Result<()> {
    ensure!(
        message.to_vec()?.len() <= 4096,
        "DNS query exceeds 4096 byte limit"
    );
    ensure!(
        message.metadata.message_type == MessageType::Query
            && message.metadata.op_code == OpCode::Query
            && message.queries.len() == 1
            && message.queries[0].query_class() == DNSClass::IN,
        "DNS requires a single IN question with QUERY opcode"
    );
    ensure!(
        message.answers.is_empty() && message.authorities.is_empty(),
        "DNS query contains answer records"
    );
    ensure!(
        message.signature.is_none(),
        "signed DNS queries are unsupported by this forwarding service"
    );
    Ok(())
}
pub fn response(query: &Message, code: ResponseCode) -> Message {
    let mut reply = Message::query();
    reply.metadata.id = query.metadata.id;
    reply.metadata.message_type = MessageType::Response;
    reply.metadata.op_code = query.metadata.op_code;
    reply.metadata.recursion_desired = query.metadata.recursion_desired;
    reply.metadata.recursion_available = true;
    reply.metadata.response_code = code;
    reply.queries = query.queries.clone();
    if let Some(edns) = &query.edns {
        let mut reply_edns = hickory_proto::op::Edns::new();
        reply_edns.set_max_payload(edns.max_payload().clamp(512, 1232));
        reply.edns = Some(reply_edns);
    }
    reply
}
pub fn validate_response(query: &Message, reply: &Message) -> Result<()> {
    ensure!(
        reply.metadata.message_type == MessageType::Response
            && reply.metadata.op_code == query.metadata.op_code
            && reply.metadata.id == query.metadata.id
            && reply.queries == query.queries,
        "DNS upstream response does not match query"
    );
    Ok(())
}
pub fn error(raw: &[u8]) -> Option<Vec<u8>> {
    let query = Message::from_vec(raw).ok()?;
    let code = if validate_query(&query).is_ok() {
        ResponseCode::ServFail
    } else {
        ResponseCode::FormErr
    };
    response(&query, code).to_vec().ok()
}
pub fn udp_reply(query: &[u8], reply: &[u8]) -> Result<Vec<u8>> {
    let limit = Message::from_vec(query)?
        .edns
        .map(|edns| edns.max_payload().clamp(512, 1232) as usize)
        .unwrap_or(512);
    if reply.len() <= limit {
        return Ok(reply.to_vec());
    }
    let mut message = Message::from_vec(reply)?;
    message.answers.clear();
    message.authorities.clear();
    message.additionals.clear();
    message.metadata.truncation = true;
    Ok(message.to_vec()?)
}
