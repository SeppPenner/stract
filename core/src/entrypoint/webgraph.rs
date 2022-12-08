// Cuely is an open source web search engine.
// Copyright (C) 2022 Cuely ApS
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
use crate::{
    entrypoint::async_download_all_warc_files,
    mapreduce::{Manager, Map, Reduce, StatelessWorker, Worker},
    warc::WarcFile,
    webgraph::{self, FrozenWebgraph, Node, WebgraphBuilder},
    webpage::Html,
    HttpConfig, LocalConfig, Result, WarcSource, WebgraphLocalConfig, WebgraphMasterConfig,
};
use futures::StreamExt;
use itertools::Itertools;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, path::Path};
use tokio::pin;
use tracing::{info, trace};

#[derive(Debug, Serialize, Deserialize, Clone)]
struct GraphPointer {
    path: String,
    with_full_path: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum JobConfig {
    Http(HttpConfig),
    Local(LocalConfig),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Job {
    pub config: JobConfig,
    pub warc_paths: Vec<String>,
    pub graph_base_path: String,
    pub create_full_graph: bool,
}

fn open_graph<P: AsRef<Path>>(path: P, with_full_graph: bool) -> webgraph::Webgraph {
    let mut builder = WebgraphBuilder::new(path).with_host_graph();

    if with_full_graph {
        builder = builder.with_full_graph()
    }

    builder.open()
}

async fn async_process_job(job: &Job) -> webgraph::Webgraph {
    let name = job.warc_paths.first().unwrap().split('/').last().unwrap();

    info!("processing {}", name);

    let mut graph = open_graph(
        Path::new(&job.graph_base_path).join(name),
        job.create_full_graph,
    );

    let source = match job.config.clone() {
        JobConfig::Http(config) => WarcSource::HTTP(config),
        JobConfig::Local(config) => WarcSource::Local(config),
    };

    let warc_files =
        async_download_all_warc_files(&job.warc_paths, &source, &job.graph_base_path).await;
    pin!(warc_files);

    while let Some(warc_path) = warc_files.next().await {
        let name = warc_path.split('/').last().unwrap();
        let path = Path::new(&job.graph_base_path)
            .join("warc_files")
            .join(name);

        if let Ok(file) = WarcFile::open(&path) {
            for record in file.records().flatten() {
                let webpage = Html::parse_without_text(&record.response.body, &record.request.url);
                for link in webpage
                    .links()
                    .into_iter()
                    .filter(|link| matches!(link.destination.protocol(), "http" | "https"))
                    .filter(|link| link.source.domain() != link.destination.domain())
                    .filter(|link| link.matches_url_regex())
                {
                    trace!("inserting link {:?}", link);
                    graph.insert(
                        Node::from(link.source),
                        Node::from(link.destination),
                        link.text,
                    );
                }
            }
        }

        graph.flush();

        std::fs::remove_file(path).unwrap();
    }

    info!("{} done", name);

    graph
}

pub fn process_job(job: &Job) -> webgraph::Webgraph {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async { async_process_job(job).await })
}

impl Map<StatelessWorker, FrozenWebgraph> for Job {
    fn map(&self, _worker: &StatelessWorker) -> FrozenWebgraph {
        let graph = process_job(self);
        graph.into()
    }
}

impl Map<StatelessWorker, GraphPointer> for Job {
    fn map(&self, _worker: &StatelessWorker) -> GraphPointer {
        let graph = process_job(self);
        GraphPointer {
            path: graph.path,
            with_full_path: self.create_full_graph,
        }
    }
}

impl Reduce<FrozenWebgraph> for webgraph::Webgraph {
    fn reduce(self, other: FrozenWebgraph) -> webgraph::Webgraph {
        let other: webgraph::Webgraph = other.into();
        self.reduce(other)
    }
}

impl Reduce<webgraph::Webgraph> for webgraph::Webgraph {
    fn reduce(mut self, element: webgraph::Webgraph) -> Self {
        let other_path = element.path.clone();

        self.merge(element);

        std::fs::remove_dir_all(other_path).unwrap();
        self
    }
}

impl Reduce<GraphPointer> for GraphPointer {
    fn reduce(self, other: GraphPointer) -> Self {
        let self_clone = self.clone();

        {
            let graph = open_graph(self.path, self.with_full_path);
            let other_graph = open_graph(other.path, self.with_full_path);

            let _ = graph.reduce(other_graph);
        }

        self_clone
    }
}

impl Reduce<GraphPointer> for webgraph::Webgraph {
    fn reduce(self, other: GraphPointer) -> Self {
        let other = open_graph(other.path, other.with_full_path);
        self.reduce(other)
    }
}

pub struct Webgraph {}

impl Webgraph {
    pub fn run_master(config: &WebgraphMasterConfig) -> Result<()> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                info!("Running master for webgraph construction");

                let warc_paths = config.warc_source.paths().unwrap();

                let workers: Vec<SocketAddr> = config
                    .workers
                    .iter()
                    .map(|worker| worker.parse().unwrap())
                    .collect();

                let job_config = match config.warc_source.clone() {
                    WarcSource::HTTP(config) => JobConfig::Http(config),
                    WarcSource::Local(config) => JobConfig::Local(config),
                };

                let mut warc_paths: Box<dyn Iterator<Item = Job> + Send> = Box::new(
                    warc_paths
                        .into_iter()
                        .chunks(config.batch_size.unwrap_or(1))
                        .into_iter()
                        .map(|warc_paths| Job {
                            config: job_config.clone(),
                            warc_paths: warc_paths.into_iter().collect(),
                            graph_base_path: config
                                .graph_base_path
                                .clone()
                                .unwrap_or_else(|| "data/webgraph".to_string()),
                            create_full_graph: config.create_full_graph.unwrap_or(true),
                        })
                        .collect::<Vec<_>>()
                        .into_iter(),
                );

                if let Some(limit) = config.limit_warc_files {
                    warc_paths = Box::new(warc_paths.take(limit));
                }

                let manager = Manager::new(&workers);
                let _graph: webgraph::Webgraph = manager
                    .run::<StatelessWorker, Job, webgraph::FrozenWebgraph, webgraph::Webgraph>(
                        warc_paths,
                    )
                    .await
                    .unwrap();
            });

        Ok(())
    }

    pub fn run_worker(worker_addr: String) -> Result<()> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                StatelessWorker::default()
                    .run::<Job, FrozenWebgraph>(
                        worker_addr
                            .parse::<SocketAddr>()
                            .expect("Could not parse worker address"),
                    )
                    .await
                    .unwrap();
            });
        Ok(())
    }

    pub fn run_locally(config: &WebgraphLocalConfig) -> Result<()> {
        let warc_paths = config.warc_source.paths()?;

        let job_config = match config.warc_source.clone() {
            WarcSource::HTTP(config) => JobConfig::Http(config),
            WarcSource::Local(config) => JobConfig::Local(config),
        };
        let worker = StatelessWorker::default();

        let mut graphs: Vec<_> = warc_paths
            .into_iter()
            .take(config.limit_warc_files.unwrap_or(usize::MAX))
            .chunks(config.batch_size.unwrap_or(1))
            .into_iter()
            .map(|warc_paths| Job {
                config: job_config.clone(),
                warc_paths: warc_paths.collect_vec(),
                graph_base_path: config
                    .graph_base_path
                    .clone()
                    .unwrap_or_else(|| "data/webgraph".to_string()),
                create_full_graph: config.create_full_graph.unwrap_or(true),
            })
            .collect_vec()
            .into_par_iter()
            .map(|job| -> GraphPointer { job.map(&worker) })
            .collect();

        if graphs.len() > 1 {
            let pointer = graphs.pop().unwrap();
            let mut graph = open_graph(pointer.path, pointer.with_full_path);
            for other in graphs {
                graph = graph.reduce(other);
            }
        }

        Ok(())
    }
}