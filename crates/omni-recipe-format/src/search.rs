//! The search tokeniser the server index and the app's in-memory filter
//! share, so both match the same way.

use crate::index::{Index, Node};
use crate::model::RecipeDoc;
use crate::text::placeholders;

/// Lowercases and strips diacritics from Latin letters: "Ragù" is "ragu",
/// "Crème Brûlée" is "creme brulee", "Weißwurst" is "weisswurst".
pub fn fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars().flat_map(char::to_lowercase) {
        match c {
            'à'..='å' | 'ā' | 'ă' | 'ą' => out.push('a'),
            'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => out.push('c'),
            'ď' | 'đ' | 'ð' => out.push('d'),
            'è'..='ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => out.push('e'),
            'ĝ' | 'ğ' | 'ġ' | 'ģ' => out.push('g'),
            'ĥ' | 'ħ' => out.push('h'),
            'ì'..='ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => out.push('i'),
            'ĵ' => out.push('j'),
            'ķ' => out.push('k'),
            'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => out.push('l'),
            'ñ' | 'ń' | 'ņ' | 'ň' => out.push('n'),
            'ò'..='ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => out.push('o'),
            'ŕ' | 'ŗ' | 'ř' => out.push('r'),
            'ś' | 'ŝ' | 'ş' | 'š' | 'ș' => out.push('s'),
            'ţ' | 'ť' | 'ŧ' | 'ț' => out.push('t'),
            'ù'..='ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => out.push('u'),
            'ŵ' => out.push('w'),
            'ý' | 'ÿ' | 'ŷ' => out.push('y'),
            'ź' | 'ż' | 'ž' => out.push('z'),
            'ß' => out.push_str("ss"),
            'æ' => out.push_str("ae"),
            'œ' => out.push_str("oe"),
            'þ' => out.push_str("th"),
            // Combining marks, as left by decomposed text.
            '\u{300}'..='\u{36f}' => {}
            other => out.push(other),
        }
    }
    out
}

/// The folded words of `text`: runs of letters and digits. Apostrophes inside
/// a word are dropped ("baker's" is "bakers"); everything else separates.
pub fn tokens(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut word = String::new();
    for c in fold(text).chars() {
        if c.is_alphanumeric() {
            word.push(c);
        } else if !matches!(c, '\'' | '’') && !word.is_empty() {
            found.push(std::mem::take(&mut word));
        }
    }
    if !word.is_empty() {
        found.push(word);
    }
    found
}

/// The words a recipe is found by: title, summary, source name and author,
/// component and ingredient names, and step text with each placeholder
/// replaced by its label or by the name of what it refers to. One item per
/// line; amounts are left out.
pub fn search_text(doc: &RecipeDoc) -> String {
    let ix = Index::build(doc);
    let mut lines: Vec<String> = vec![doc.title.clone()];
    lines.extend(doc.summary.clone());
    lines.extend(doc.source.name.clone());
    lines.extend(doc.source.author.clone());
    for component in &doc.components {
        lines.extend(component.name.clone());
        lines.extend(component.ingredients.iter().map(|i| i.name.clone()));
        for step in &component.steps {
            let mut text = String::new();
            let mut at = 0;
            for placeholder in placeholders(&step.text) {
                text.push_str(&step.text[at..placeholder.start]);
                at = placeholder.end;
                let named = match ix.node(placeholder.r#ref) {
                    Some(Node::Ingredient(ingredient)) => Some(ingredient.name.as_str()),
                    Some(Node::Step(step)) => step.output.as_deref(),
                    Some(Node::Byproduct(byproduct)) => Some(byproduct.name.as_str()),
                    Some(Node::Component(component)) => component.name.as_deref(),
                    None => None,
                };
                text.push_str(placeholder.label.or(named).unwrap_or_default());
            }
            text.push_str(&step.text[at..]);
            lines.push(text);
        }
    }
    lines.retain(|line| !line.trim().is_empty());
    lines.join("\n")
}

/// Whether every query token is the start of some token of the haystack, so
/// "chick pic" finds "Chicken Piccata". An empty query matches everything.
pub fn matches<Q: AsRef<str>, H: AsRef<str>>(query_tokens: &[Q], haystack_tokens: &[H]) -> bool {
    query_tokens.iter().all(|query| {
        haystack_tokens
            .iter()
            .any(|token| token.as_ref().starts_with(query.as_ref()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folding_strips_case_and_diacritics() {
        assert_eq!(fold("Ragù alla Bolognese"), "ragu alla bolognese");
        assert_eq!(fold("Crème Brûlée"), "creme brulee");
        assert_eq!(
            fold("Weißwurst, Smørrebrød, Œuf"),
            "weisswurst, smorrebrod, oeuf"
        );
        assert_eq!(fold("Be\u{301}chamel ÀÉÎÕÜ"), "bechamel aeiou");
        assert_eq!(fold("Żurek Łódź"), "zurek lodz");
    }

    #[test]
    fn tokens_are_runs_of_letters_and_digits() {
        assert_eq!(
            tokens("All-purpose flour, 12-inch (baker's) Béchamel"),
            [
                "all", "purpose", "flour", "12", "inch", "bakers", "bechamel"
            ]
        );
        assert_eq!(tokens("  --  "), Vec::<String>::new());
    }

    #[test]
    fn matching_is_by_prefix_of_every_query_token() {
        let haystack = tokens("Chicken Piccata with lemon");
        assert!(matches(&tokens("chick pic"), &haystack));
        assert!(matches(&tokens("LEMON"), &haystack));
        assert!(matches(&tokens(""), &haystack));
        assert!(!matches(&tokens("chick beef"), &haystack));
        assert!(!matches(&tokens("icken"), &haystack));
    }
}
