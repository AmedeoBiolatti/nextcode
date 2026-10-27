use extension_sdk::{Invocation, Request, Response, TextEdit, export_extension};

export_extension!(handle);

fn handle(request: Request) -> Response {
    if !matches!(&request.invocation, Invocation::Command { id } if id == "uppercase-selection") {
        return error("Unknown command: expected uppercase-selection");
    }
    let Some(document) = request.document else {
        return error("Open a document and select text before running Uppercase Selection");
    };
    if document.text.is_empty() {
        return error("The document is empty; select some text to uppercase");
    }
    let selection = document.selection;
    if selection.start == selection.end {
        return error("Select some text before running Uppercase Selection");
    }
    let Some(selected) = document
        .text
        .get(selection.start as usize..selection.end as usize)
    else {
        return error("The selection is not a valid UTF-8 byte range in the document");
    };

    Response {
        edits: vec![TextEdit {
            start: selection.start,
            end: selection.end,
            text: selected.to_uppercase(),
        }],
        logs: vec!["Uppercased the selected text".to_owned()],
        error: None,
        panels: Vec::new(),
    }
}

fn error(message: &str) -> Response {
    Response {
        error: Some(message.to_owned()),
        ..Response::default()
    }
}
