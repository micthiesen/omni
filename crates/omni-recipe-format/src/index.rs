//! Id lookup over a document: what each id names, where it lives, and which
//! steps consume it. Total on any document; a duplicated id resolves to its
//! first use.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::model::{Byproduct, Component, Ingredient, Input, RecipeDoc, Step};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Ingredient,
    Step,
    Byproduct,
    Component,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ingredient => "ingredient",
            Self::Step => "step",
            Self::Byproduct => "byproduct",
            Self::Component => "component",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Node<'a> {
    Ingredient(&'a Ingredient),
    Step(&'a Step),
    Byproduct(&'a Byproduct),
    Component(&'a Component),
}

impl Node<'_> {
    pub fn kind(&self) -> Kind {
        match self {
            Self::Ingredient(_) => Kind::Ingredient,
            Self::Step(_) => Kind::Step,
            Self::Byproduct(_) => Kind::Byproduct,
            Self::Component(_) => Kind::Component,
        }
    }
}

/// One use of an id: the consuming step and its input.
#[derive(Debug, Clone, Copy)]
pub struct Use<'a> {
    pub step: &'a Step,
    pub input: &'a Input,
}

#[derive(Debug, Clone, Copy)]
struct Entry<'a> {
    node: Node<'a>,
    component: &'a Component,
}

#[derive(Debug, Clone)]
pub struct Index<'a> {
    doc: &'a RecipeDoc,
    ids: Vec<&'a str>,
    entries: HashMap<&'a str, Entry<'a>>,
    step_pos: HashMap<&'a str, usize>,
    producer: HashMap<&'a str, &'a str>,
    consumed: Vec<&'a str>,
    consumers: HashMap<&'a str, Vec<Use<'a>>>,
    duplicates: Vec<&'a str>,
}

impl<'a> Index<'a> {
    pub fn build(doc: &'a RecipeDoc) -> Self {
        let mut ix = Self {
            doc,
            ids: Vec::new(),
            entries: HashMap::new(),
            step_pos: HashMap::new(),
            producer: HashMap::new(),
            consumed: Vec::new(),
            consumers: HashMap::new(),
            duplicates: Vec::new(),
        };
        let mut position = 0;
        for component in &doc.components {
            ix.add(&component.id, Node::Component(component), component);
            for ingredient in &component.ingredients {
                ix.add(&ingredient.id, Node::Ingredient(ingredient), component);
            }
            for step in &component.steps {
                ix.add(&step.id, Node::Step(step), component);
                ix.step_pos.insert(&step.id, position);
                position += 1;
                for byproduct in &step.byproducts {
                    ix.add(&byproduct.id, Node::Byproduct(byproduct), component);
                    ix.producer.insert(&byproduct.id, &step.id);
                }
            }
        }
        for component in &doc.components {
            for step in &component.steps {
                for input in &step.inputs {
                    let uses = ix.consumers.entry(&input.r#ref).or_default();
                    if uses.is_empty() {
                        ix.consumed.push(&input.r#ref);
                    }
                    uses.push(Use { step, input });
                }
            }
        }
        ix
    }

    fn add(&mut self, id: &'a str, node: Node<'a>, component: &'a Component) {
        if self.entries.contains_key(id) {
            self.duplicates.push(id);
            return;
        }
        self.ids.push(id);
        self.entries.insert(id, Entry { node, component });
    }

    pub fn doc(&self) -> &'a RecipeDoc {
        self.doc
    }

    /// Every distinct id, in document order.
    pub fn ids(&self) -> &[&'a str] {
        &self.ids
    }

    /// Ids defined more than once, one entry per extra definition.
    pub fn duplicates(&self) -> &[&'a str] {
        &self.duplicates
    }

    pub fn contains(&self, id: &str) -> bool {
        self.entries.contains_key(id)
    }

    pub fn node(&self, id: &str) -> Option<Node<'a>> {
        self.entries.get(id).map(|entry| entry.node)
    }

    pub fn kind(&self, id: &str) -> Option<Kind> {
        self.node(id).map(|node| node.kind())
    }

    pub fn ingredient(&self, id: &str) -> Option<&'a Ingredient> {
        match self.node(id) {
            Some(Node::Ingredient(ingredient)) => Some(ingredient),
            _ => None,
        }
    }

    pub fn step(&self, id: &str) -> Option<&'a Step> {
        match self.node(id) {
            Some(Node::Step(step)) => Some(step),
            _ => None,
        }
    }

    /// The component `id` is, or belongs to.
    pub fn component_of(&self, id: &str) -> Option<&'a Component> {
        self.entries.get(id).map(|entry| entry.component)
    }

    /// The position of a step among all steps in written order.
    pub fn step_pos(&self, id: &str) -> Option<usize> {
        self.step_pos.get(id).copied()
    }

    /// The step that yields a by-product.
    pub fn producer(&self, id: &str) -> Option<&'a str> {
        self.producer.get(id).copied()
    }

    /// The uses of `id`, in document order.
    pub fn consumers(&self, id: &str) -> &[Use<'a>] {
        self.consumers.get(id).map_or(&[], Vec::as_slice)
    }

    /// Every id some step takes as an input, in order of first use. Includes
    /// refs that resolve to nothing.
    pub fn consumed(&self) -> &[&'a str] {
        &self.consumed
    }

    /// Every step with its component, in written order.
    pub fn steps(&self) -> impl Iterator<Item = (&'a Component, &'a Step)> + use<'a> {
        self.doc
            .components
            .iter()
            .flat_map(|component| component.steps.iter().map(move |step| (component, step)))
    }

    /// The step that must be done before `id` is available: the step itself,
    /// a by-product's producer, or a component's last step. Ingredients and
    /// unknown ids have none.
    pub fn source_step(&self, id: &str) -> Option<&'a str> {
        match self.node(id)? {
            Node::Step(step) => Some(&step.id),
            Node::Byproduct(_) => self.producer(id),
            Node::Component(component) => component.steps.last().map(|step| step.id.as_str()),
            Node::Ingredient(_) => None,
        }
    }
}
