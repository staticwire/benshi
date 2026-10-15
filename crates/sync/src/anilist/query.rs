//! What the backend asks AniList, and the shape of what it answers.

use serde::Deserialize;
use serde_json::Value;

/// The account the token is of.
pub(super) const VIEWER: &str = "query { Viewer { id name } }";

/// One chunk of an account's list of anime, with what each entry holds.
///
/// The score is asked for in a hundred points, whatever format the account
/// shows it in.
pub(super) const LIST: &str = "query ($user: Int, $chunk: Int) { \
    MediaListCollection(userId: $user, type: ANIME, chunk: $chunk) { \
    hasNextChunk lists { isCustomList entries { \
    mediaId status progress repeat score(format: POINT_100) \
    media { format episodes duration status seasonYear startDate { year } \
    title { userPreferred romaji english native } synonyms } } } } }";

/// An answer's body: its data, or the errors it carries in place of it, or
/// both.
///
/// The data is read as it stands before it is read as what was asked for: an
/// answer that carries an error can hold the data asked for with a field of
/// it null, and the error is what it says.
#[derive(Debug, Deserialize)]
pub(super) struct Envelope {
    pub data: Option<Value>,
    #[serde(default)]
    pub errors: Vec<Problem>,
}

/// One error an answer carries.
#[derive(Debug, Deserialize)]
pub(super) struct Problem {
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct ViewerData {
    #[serde(rename = "Viewer")]
    pub viewer: Viewer,
}

#[derive(Debug, Deserialize)]
pub(super) struct Viewer {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct ListData {
    #[serde(rename = "MediaListCollection")]
    pub collection: Collection,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Collection {
    pub has_next_chunk: bool,
    pub lists: Vec<List>,
}

/// One of the lists an account's anime are filed in: by status, by format
/// where the account splits its completed list that way, or one of the
/// account's own making.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct List {
    pub is_custom_list: bool,
    pub entries: Vec<ListEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ListEntry {
    pub media_id: i64,
    pub status: ListStatus,
    pub progress: Option<u32>,
    pub repeat: Option<u32>,
    pub score: Option<u8>,
    pub media: Media,
}

/// `MediaListStatus`.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(super) enum ListStatus {
    Current,
    Planning,
    Completed,
    Dropped,
    Paused,
    Repeating,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Media {
    pub format: Option<String>,
    pub episodes: Option<u32>,
    pub duration: Option<u32>,
    pub status: Option<String>,
    pub season_year: Option<u16>,
    pub start_date: Option<FuzzyDate>,
    pub title: Title,
    pub synonyms: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub(super) struct FuzzyDate {
    pub year: Option<u16>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Title {
    pub user_preferred: Option<String>,
    pub romaji: Option<String>,
    pub english: Option<String>,
    pub native: Option<String>,
}
