use serde_json::value::to_raw_value;

use super::{TerminalToolReceipt, TerminalToolReceiptError, ToolOutputBody};

#[test]
fn terminal_receipts_reject_unbounded_output_and_metadata() {
    let output_error = TerminalToolReceipt::new(
        "call-1".to_owned(),
        "finish".to_owned(),
        ToolOutputBody::Text("x".repeat(TerminalToolReceipt::MAX_OUTPUT_BYTES * 2)),
        None,
    )
    .unwrap_err();
    assert_eq!(output_error, TerminalToolReceiptError::OutputTooLarge);

    let metadata_error = TerminalToolReceipt::new(
        "call-1".to_owned(),
        "finish".to_owned(),
        ToolOutputBody::Text("ok".to_owned()),
        Some(to_raw_value(&"x".repeat(TerminalToolReceipt::MAX_METADATA_BYTES * 2)).unwrap()),
    )
    .unwrap_err();
    assert_eq!(metadata_error, TerminalToolReceiptError::MetadataTooLarge);
}
