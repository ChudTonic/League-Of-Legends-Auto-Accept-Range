//! Single source of truth for League's "forms"/HOL-chroma special cases —
//! consolidates three duplicated Python copies (`special_cases.py`,
//! `zip_resolver.py`, `skin_display_handler.py`). These are champions whose
//! alternate skin states show as chroma-like picks but aren't real LCU
//! chromas (fake UI-only IDs like Elementalist Lux's 99991-99999, or real
//! skin IDs the LCU treats as siblings rather than chromas of their base).

#![allow(dead_code)] // consumed by S4+ (chroma panel, injection trigger)

/// One "form" or HOL-chroma entry: a UI-facing fake/real ID standing in for
/// a champion-specific alternate skin state that isn't a normal LCU chroma.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormSkin {
    pub fake_id: i64,
    pub base_id: i64,
    pub champion: &'static str,
    pub display: &'static str,
    /// Literal relative zip path. Empty for entries resolved generically by
    /// chroma ID instead (the two Risen Legend HOL chromas).
    pub zip_rel: &'static str,
}

pub const FORMS: &[FormSkin] = &[
    // Elementalist Lux — base 99007, fake IDs 99991-99999.
    FormSkin { fake_id: 99991, base_id: 99007, champion: "Lux", display: "Air", zip_rel: "Lux/Forms/Lux Elementalist Air.zip" },
    FormSkin { fake_id: 99992, base_id: 99007, champion: "Lux", display: "Dark", zip_rel: "Lux/Forms/Lux Elementalist Dark.zip" },
    FormSkin { fake_id: 99993, base_id: 99007, champion: "Lux", display: "Ice", zip_rel: "Lux/Forms/Lux Elementalist Ice.zip" },
    FormSkin { fake_id: 99994, base_id: 99007, champion: "Lux", display: "Magma", zip_rel: "Lux/Forms/Lux Elementalist Magma.zip" },
    FormSkin { fake_id: 99995, base_id: 99007, champion: "Lux", display: "Mystic", zip_rel: "Lux/Forms/Lux Elementalist Mystic.zip" },
    FormSkin { fake_id: 99996, base_id: 99007, champion: "Lux", display: "Nature", zip_rel: "Lux/Forms/Lux Elementalist Nature.zip" },
    FormSkin { fake_id: 99997, base_id: 99007, champion: "Lux", display: "Storm", zip_rel: "Lux/Forms/Lux Elementalist Storm.zip" },
    FormSkin { fake_id: 99998, base_id: 99007, champion: "Lux", display: "Water", zip_rel: "Lux/Forms/Lux Elementalist Water.zip" },
    FormSkin { fake_id: 99999, base_id: 99007, champion: "Lux", display: "Fire", zip_rel: "Lux/Forms/Elementalist Lux Fire.zip" },

    // Sahn-Uzal Mordekaiser — base 82054, real IDs 82998/82999.
    FormSkin { fake_id: 82998, base_id: 82054, champion: "Mordekaiser", display: "Form 1", zip_rel: "Mordekaiser/Forms/Sahn Uzal Mordekaiser Form 1.zip" },
    FormSkin { fake_id: 82999, base_id: 82054, champion: "Mordekaiser", display: "Form 2", zip_rel: "Mordekaiser/Forms/Sahn Uzal Mordekaiser Form 2.zip" },

    // Spirit Blossom Morgana — base 25080, real ID 25999.
    FormSkin { fake_id: 25999, base_id: 25080, champion: "Morgana", display: "Form 1", zip_rel: "Morgana/Forms/Spirit Blossom Morgana Form 1.zip" },

    // Radiant Sett — base 875066, real IDs 875998/875999.
    FormSkin { fake_id: 875998, base_id: 875066, champion: "Sett", display: "Form 2", zip_rel: "Sett/Forms/Radiant Sett Form 2.zip" },
    FormSkin { fake_id: 875999, base_id: 875066, champion: "Sett", display: "Form 3", zip_rel: "Sett/Forms/Radiant Sett Form 3.zip" },

    // KDA ALL OUT Seraphine — base 147001, real IDs 147002/147003.
    FormSkin { fake_id: 147002, base_id: 147001, champion: "Seraphine", display: "Form 1", zip_rel: "Seraphine/Forms/KDA Seraphine Form 1.zip" },
    FormSkin { fake_id: 147003, base_id: 147001, champion: "Seraphine", display: "Form 2", zip_rel: "Seraphine/Forms/KDA Seraphine Form 2.zip" },

    // Viego — base 234043, real IDs 234994-234999.
    FormSkin { fake_id: 234994, base_id: 234043, champion: "Viego", display: "Form 2", zip_rel: "Viego/Forms/Viego Form 2.zip" },
    FormSkin { fake_id: 234995, base_id: 234043, champion: "Viego", display: "Form 3", zip_rel: "Viego/Forms/Viego Form 3.zip" },
    FormSkin { fake_id: 234996, base_id: 234043, champion: "Viego", display: "Form 4", zip_rel: "Viego/Forms/Viego Form 4.zip" },
    FormSkin { fake_id: 234997, base_id: 234043, champion: "Viego", display: "Form 5", zip_rel: "Viego/Forms/Viego Form 5.zip" },
    FormSkin { fake_id: 234998, base_id: 234043, champion: "Viego", display: "Form 6", zip_rel: "Viego/Forms/Viego Form 6.zip" },
    FormSkin { fake_id: 234999, base_id: 234043, champion: "Viego", display: "Form 7", zip_rel: "Viego/Forms/Viego Form 7.zip" },

    // Risen Legend Kai'Sa HOL chroma — base 145070, real ID 145071.
    // Falls through to the generic champion/skin/chroma directory scan by ID.
    FormSkin { fake_id: 145071, base_id: 145070, champion: "Kai'Sa", display: "Immortalized Legend", zip_rel: "" },

    // Risen Legend Ahri HOL chromas — base 103085, real IDs 103086/103087.
    // Same generic-scan resolution as the Kai'Sa entry above.
    FormSkin { fake_id: 103086, base_id: 103085, champion: "Ahri", display: "Immortalized Legend", zip_rel: "" },
    FormSkin { fake_id: 103087, base_id: 103085, champion: "Ahri", display: "Form 2", zip_rel: "" },
];

pub fn form_by_id(id: i64) -> Option<&'static FormSkin> {
    FORMS.iter().find(|f| f.fake_id == id)
}

pub fn is_special_id(id: i64) -> bool {
    form_by_id(id).is_some()
}

pub fn base_for(id: i64) -> Option<i64> {
    form_by_id(id).map(|f| f.base_id)
}

/// Champion ID implied by an LCU skin ID (magic value preserved verbatim
/// from the Python original: base skin id = champion_id * 1000 + variant index).
pub fn champion_of(skin_id: i64) -> i64 {
    skin_id / 1000
}

/// True when `skin_id` is a champion's base skin (variant index 0).
pub fn is_base(skin_id: i64) -> bool {
    skin_id % 1000 == 0
}

/// True when `chroma` is a normal LCU chroma of `base` (chroma window
/// `base+1..=base+99`, magic value preserved verbatim from the Python original).
pub fn is_chroma_of(chroma: i64, base: i64) -> bool {
    base < chroma && chroma < base + 100
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    #[test]
    fn every_fake_id_agrees_with_its_base_on_champion() {
        let mut champion_by_base: HashMap<i64, &'static str> = HashMap::new();
        for form in FORMS {
            let champ = *champion_by_base.entry(form.base_id).or_insert(form.champion);
            assert_eq!(
                champ, form.champion,
                "fake_id {} champion {:?} disagrees with base {} champion {:?}",
                form.fake_id, form.champion, form.base_id, champ
            );
        }
    }

    #[test]
    fn fake_ids_are_unique() {
        let mut seen = HashSet::new();
        for form in FORMS {
            assert!(seen.insert(form.fake_id), "duplicate fake_id {}", form.fake_id);
        }
    }

    #[test]
    fn lookup_helpers_agree_with_the_table() {
        for form in FORMS {
            assert!(is_special_id(form.fake_id));
            assert_eq!(base_for(form.fake_id), Some(form.base_id));
            assert_eq!(form_by_id(form.fake_id), Some(form));
        }
        assert!(!is_special_id(0));
        assert!(!is_special_id(99007)); // the base itself isn't a fake/form id
    }

    #[test]
    fn base_skin_math_matches_magic_values() {
        assert_eq!(champion_of(99000), 99);
        assert!(is_base(99000));
        assert!(!is_base(99001));
        assert!(is_chroma_of(99050, 99000));
        assert!(!is_chroma_of(99100, 99000)); // window is base+1..base+99
        assert!(!is_chroma_of(99000, 99000)); // base is not its own chroma
    }
}
