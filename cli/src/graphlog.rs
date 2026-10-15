// Copyright 2020 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::hash::Hash;
use std::io;
use std::io::Write;

use jj_lib::config::ConfigGetError;
use jj_lib::graph::GraphEdge;
use jj_lib::graph::GraphEdgeType;
use jj_lib::settings::UserSettings;
use renderdag::Ancestor;
use renderdag::GraphRowRenderer as RenderDagGraphRowRenderer;
use renderdag::Renderer;

pub trait GraphLog<K: Clone + Eq + Hash> {
    fn add_node(
        &mut self,
        id: &K,
        edges: &[GraphEdge<K>],
        node_symbol: &str,
        text: &str,
    ) -> io::Result<()>;

    fn width(&self, id: &K, edges: &[GraphEdge<K>]) -> usize;
}

struct WriterGraphLog<'writer, K: Clone + Eq + Hash> {
    renderer: Box<dyn GraphRenderer<K> + 'writer>,
    writer: &'writer mut dyn Write,
}

fn convert_graph_edge_into_ancestor<K: Clone>(e: &GraphEdge<K>) -> Ancestor<K> {
    match e.edge_type {
        GraphEdgeType::Direct => Ancestor::Parent(e.target.clone()),
        GraphEdgeType::Indirect => Ancestor::Ancestor(e.target.clone()),
        GraphEdgeType::Missing => Ancestor::Anonymous,
    }
}

impl<K: Clone + Eq + Hash> GraphLog<K> for WriterGraphLog<'_, K> {
    fn add_node(
        &mut self,
        id: &K,
        edges: &[GraphEdge<K>],
        node_symbol: &str,
        text: &str,
    ) -> io::Result<()> {
        let row = self.renderer.render_row(id, edges, node_symbol, text);
        write!(self.writer, "{row}")
    }

    fn width(&self, id: &K, edges: &[GraphEdge<K>]) -> usize {
        self.renderer.width(id, edges)
    }
}

/// Stateful renderer for one revision graph row at a time.
///
/// This is useful for callers such as terminal UIs which need to retain each
/// rendered row separately instead of writing the entire graph to one stream.
pub trait GraphRenderer<K: Clone + Eq + Hash> {
    fn render_row(
        &mut self,
        id: &K,
        edges: &[GraphEdge<K>],
        node_symbol: &str,
        text: &str,
    ) -> String;

    fn width(&self, id: &K, edges: &[GraphEdge<K>]) -> usize;
}

struct SaplingGraphRenderer<R> {
    renderer: R,
}

impl<K, R> GraphRenderer<K> for SaplingGraphRenderer<R>
where
    K: Clone + Eq + Hash,
    R: Renderer<K, Output = String>,
{
    fn render_row(
        &mut self,
        id: &K,
        edges: &[GraphEdge<K>],
        node_symbol: &str,
        text: &str,
    ) -> String {
        self.renderer.next_row(
            id.clone(),
            edges.iter().map(convert_graph_edge_into_ancestor).collect(),
            node_symbol.into(),
            text.into(),
        )
    }

    fn width(&self, id: &K, edges: &[GraphEdge<K>]) -> usize {
        let parents = edges.iter().map(convert_graph_edge_into_ancestor).collect();
        let width: u64 = self.renderer.width(Some(id), Some(&parents));
        width.try_into().unwrap()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(rename_all(deserialize = "kebab-case"))]
pub enum GraphStyle {
    Ascii,
    AsciiLarge,
    Curved,
    Square,
}

impl GraphStyle {
    pub fn from_settings(settings: &UserSettings) -> Result<Self, ConfigGetError> {
        settings.get("ui.graph.style")
    }
}

pub fn get_graphlog<'a, K: Clone + Eq + Hash + 'a>(
    style: GraphStyle,
    formatter: &'a mut dyn Write,
) -> Box<dyn GraphLog<K> + 'a> {
    Box::new(WriterGraphLog {
        renderer: get_graph_renderer(style),
        writer: formatter,
    })
}

pub fn get_graph_renderer<'a, K: Clone + Eq + Hash + 'a>(
    style: GraphStyle,
) -> Box<dyn GraphRenderer<K> + 'a> {
    let builder = RenderDagGraphRowRenderer::new()
        .output()
        .with_min_row_height(0);
    match style {
        GraphStyle::Ascii => Box::new(SaplingGraphRenderer {
            renderer: builder.build_ascii(),
        }),
        GraphStyle::AsciiLarge => Box::new(SaplingGraphRenderer {
            renderer: builder.build_ascii_large(),
        }),
        GraphStyle::Curved => Box::new(SaplingGraphRenderer {
            renderer: builder.build_box_drawing(),
        }),
        GraphStyle::Square => Box::new(SaplingGraphRenderer {
            renderer: builder.build_box_drawing().with_square_glyphs(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_graph_renderer_renders_revision_topology() {
        let rows = [
            (
                3,
                vec![GraphEdge::direct(1), GraphEdge::direct(2)],
                "@",
                "merge",
            ),
            (2, vec![GraphEdge::direct(0)], "o", "right"),
            (1, vec![GraphEdge::direct(0)], "o", "left"),
            (0, vec![], "o", "root"),
        ];
        let mut graph = get_graph_renderer(GraphStyle::Ascii);
        let output: String = rows
            .iter()
            .map(|(id, edges, node_symbol, text)| graph.render_row(id, edges, node_symbol, text))
            .collect();
        assert_eq!(
            output,
            "@    merge\n|\\\n| o  right\no |  left\n|/\no  root\n"
        );
    }
}
