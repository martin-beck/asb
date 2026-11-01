//! Provider-owned model catalog schema defaults.
use super::super::{CatalogPagination, CatalogSchema, ModelKind};

pub(in crate::providers) fn schema(kind: ModelKind) -> CatalogSchema {
    let collection = match kind {
        ModelKind::ModelsPageTokenApiKey
        | ModelKind::ModelsPageTokenBearer
        | ModelKind::ModelsUnpaginated => "models",
        _ => "data",
    };
    let pagination = match kind {
        ModelKind::DataCursor => CatalogPagination::Flagged {
            more: "has_more",
            cursor: "last_id",
            query: "after_id",
        },
        ModelKind::ModelsPageTokenApiKey | ModelKind::ModelsPageTokenBearer => {
            CatalogPagination::Token {
                field: "nextPageToken",
                query: "pageToken",
            }
        }
        _ => CatalogPagination::None,
    };
    CatalogSchema {
        collection,
        allow_top_level_array: kind == ModelKind::ArrayUnpaginated,
        pagination,
    }
}
