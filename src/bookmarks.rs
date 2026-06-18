use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// A single entry extracted from the PDF's internal outline (bookmarks / TOC).
/// Mirrors the PDF outline tree — each entry may have sub-entries.
#[derive(Debug, Clone)]
pub struct PdfOutlineEntry {
    /// The display title of this outline item.
    pub title: String,
    /// 0-based page index this item links to, if resolvable.
    pub page: Option<usize>,
    /// Nested children (sub-sections).
    pub children: Vec<PdfOutlineEntry>,
}

/// A native PDF bookmark entry to be baked into the PDF /Outline catalog.
/// Distinct from `LinkData` which is the app-level serde bookmark (file path,
/// tags, icon, etc.).  Only entries with a known `target_page` can be written
/// as native outlines; file-only bookmarks stay in `bookmarks.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PdfNativeBookmark {
    pub title: String,
    /// 0-based page index for pdfium / lopdf.
    pub target_page: usize,
    pub children: Vec<PdfNativeBookmark>,
}


#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LinkData {
    pub id: String,
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub page: Option<usize>,
    pub icon: Option<String>,
    pub tags: Vec<String>,
    pub date_added: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FolderData {
    pub id: String,
    pub title: String,
    pub children: Vec<Bookmark>,
    #[serde(default)]
    pub page: Option<usize>,
    pub date_added: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Bookmark {
    Link(LinkData),
    Folder(FolderData),
}

impl Bookmark {
    pub fn id(&self) -> &str {
        match self {
            Bookmark::Link(l) => &l.id,
            Bookmark::Folder(f) => &f.id,
        }
    }

    pub fn title(&self) -> &str {
        match self {
            Bookmark::Link(l) => &l.title,
            Bookmark::Folder(f) => &f.title,
        }
    }

    pub fn date_added(&self) -> u64 {
        match self {
            Bookmark::Link(l) => l.date_added,
            Bookmark::Folder(f) => f.date_added,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookmarkManager {
    pub root: Vec<Bookmark>,
}

impl BookmarkManager {
    pub fn new() -> Self {
        Self { root: Vec::new() }
    }

    pub fn add_bookmark(&mut self, parent_id: Option<&str>, bookmark: Bookmark) -> bool {
        if let Some(pid) = parent_id {
            Self::add_to_folder(&mut self.root, pid, bookmark)
        } else {
            self.root.push(bookmark);
            true
        }
    }

    fn add_to_folder(nodes: &mut Vec<Bookmark>, target_id: &str, bookmark: Bookmark) -> bool {
        for node in nodes.iter_mut() {
            if let Bookmark::Folder(folder) = node {
                if folder.id == target_id {
                    folder.children.push(bookmark);
                    return true;
                }
                if Self::add_to_folder(&mut folder.children, target_id, bookmark.clone()) {
                    return true;
                }
            }
        }
        false
    }

    pub fn delete_bookmark(&mut self, target_id: &str) -> bool {
        let mut found = false;
        self.root.retain(|b| {
            if b.id() == target_id {
                found = true;
                false
            } else {
                true
            }
        });
        if found { return true; }

        for node in self.root.iter_mut() {
            if let Bookmark::Folder(folder) = node {
                if Self::delete_from_folder(&mut folder.children, target_id) {
                    return true;
                }
            }
        }
        false
    }

    fn delete_from_folder(nodes: &mut Vec<Bookmark>, target_id: &str) -> bool {
        let mut found = false;
        nodes.retain(|b| {
            if b.id() == target_id {
                found = true;
                false
            } else {
                true
            }
        });
        if found { return true; }

        for node in nodes.iter_mut() {
            if let Bookmark::Folder(folder) = node {
                if Self::delete_from_folder(&mut folder.children, target_id) {
                    return true;
                }
            }
        }
        false
    }

    pub fn rename_bookmark(&mut self, target_id: &str, new_title: String) -> bool {
        Self::rename_in_nodes(&mut self.root, target_id, new_title)
    }

    fn rename_in_nodes(nodes: &mut Vec<Bookmark>, target_id: &str, new_title: String) -> bool {
        for node in nodes.iter_mut() {
            if node.id() == target_id {
                match node {
                    Bookmark::Link(l) => l.title = new_title,
                    Bookmark::Folder(f) => f.title = new_title,
                }
                return true;
            }
            if let Bookmark::Folder(folder) = node {
                if Self::rename_in_nodes(&mut folder.children, target_id, new_title.clone()) {
                    return true;
                }
            }
        }
        false
    }

    pub fn move_bookmark(&mut self, target_id: &str, new_parent_id: Option<&str>) -> bool {
        if let Some(node) = self.extract_node(target_id) {
            if let Some(pid) = new_parent_id {
                if !Self::add_to_folder(&mut self.root, pid, node.clone()) {
                    // Revert by pushing to root on failure
                    self.root.push(node);
                    return false;
                }
                return true;
            } else {
                self.root.push(node);
                return true;
            }
        }
        false
    }

    fn extract_node(&mut self, target_id: &str) -> Option<Bookmark> {
        if let Some(pos) = self.root.iter().position(|b| b.id() == target_id) {
            return Some(self.root.remove(pos));
        }
        for node in self.root.iter_mut() {
            if let Bookmark::Folder(folder) = node {
                if let Some(found) = Self::extract_from_folder(&mut folder.children, target_id) {
                    return Some(found);
                }
            }
        }
        None
    }

    fn extract_from_folder(nodes: &mut Vec<Bookmark>, target_id: &str) -> Option<Bookmark> {
        if let Some(pos) = nodes.iter().position(|b| b.id() == target_id) {
            return Some(nodes.remove(pos));
        }
        for node in nodes.iter_mut() {
            if let Bookmark::Folder(folder) = node {
                if let Some(found) = Self::extract_from_folder(&mut folder.children, target_id) {
                    return Some(found);
                }
            }
        }
        None
    }

    pub fn search(&self, query: &str) -> Vec<Bookmark> {
        let mut results = Vec::new();
        let query_lower = query.to_lowercase();
        Self::search_recursive(&self.root, &query_lower, &mut results);
        results
    }

    fn search_recursive(nodes: &[Bookmark], query: &str, results: &mut Vec<Bookmark>) {
        for node in nodes {
            let matches_title = node.title().to_lowercase().contains(query);
            let matches_tag = match node {
                Bookmark::Link(l) => l.tags.iter().any(|t| t.to_lowercase().contains(query)),
                _ => false,
            };
            
            if matches_title || matches_tag {
                results.push(node.clone());
            }

            if let Bookmark::Folder(folder) = node {
                Self::search_recursive(&folder.children, query, results);
            }
        }
    }

    pub fn sort_by_date(&mut self) {
        Self::sort_nodes_by_date(&mut self.root);
    }
    
    fn sort_nodes_by_date(nodes: &mut [Bookmark]) {
        nodes.sort_by_key(|b| std::cmp::Reverse(b.date_added()));
        for node in nodes.iter_mut() {
            if let Bookmark::Folder(folder) = node {
                Self::sort_nodes_by_date(&mut folder.children);
            }
        }
    }

    pub fn sort_alphabetically(&mut self) {
        Self::sort_nodes_alphabetically(&mut self.root);
    }

    fn sort_nodes_alphabetically(nodes: &mut [Bookmark]) {
        nodes.sort_by(|a, b| a.title().to_lowercase().cmp(&b.title().to_lowercase()));
        for node in nodes.iter_mut() {
            if let Bookmark::Folder(folder) = node {
                Self::sort_nodes_alphabetically(&mut folder.children);
            }
        }
    }

    pub fn load_from_disk(path: &Path) -> Result<Self, std::io::Error> {
        if path.exists() {
            let data = fs::read_to_string(path)?;
            let manager = serde_json::from_str(&data)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            Ok(manager)
        } else {
            Ok(Self::new())
        }
    }

    pub fn save_to_disk(&self, path: &Path) -> Result<(), std::io::Error> {
        let data = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        fs::write(path, data)?;
        Ok(())
    }
}
