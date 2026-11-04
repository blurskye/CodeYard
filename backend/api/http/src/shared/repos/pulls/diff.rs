use codeyard_webui::pages::repos::issues as view;
/// Bound the actual envelope, including drafts, pending threads, references and JSON escaping.
pub(crate) fn bound_thread_projection(
    envelope: &mut codeyard_webui::app::PageEnvelope,
) -> Result<(), serde_json::Error> {
    use codeyard_webui::app::Page;
    let entries = match &mut envelope.page {
        Page::Collaboration(model) => match model.as_mut() {
            view::Page::Diff { threads, .. } => std::mem::take(threads),
            _ => return Ok(()),
        },
        _ => return Ok(()),
    };
    let mut bytes = serde_json::to_vec(envelope)?.len();
    if bytes > codeyard_webui::bootstrap::MAX_PAGE_MODEL_BYTES {
        return Err(serde::ser::Error::custom(
            "Diff page exceeds its size limit",
        ));
    }
    let mut kept = Vec::new();
    let mut cut = false;
    for thread in entries {
        let size = serde_json::to_vec(&thread)?.len() + usize::from(!kept.is_empty());
        if bytes.saturating_add(size) > codeyard_webui::bootstrap::MAX_PAGE_MODEL_BYTES {
            cut = true;
            break;
        }
        bytes += size;
        kept.push(thread);
    }
    if let Page::Collaboration(model) = &mut envelope.page {
        if let view::Page::Diff {
            threads, truncated, ..
        } = model.as_mut()
        {
            *threads = kept;
            *truncated |= cut;
        }
    }
    Ok(())
}
