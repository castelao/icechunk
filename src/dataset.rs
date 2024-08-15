use std::{
    collections::{HashMap, HashSet},
    iter,
    sync::Arc,
};

use futures::{Stream, StreamExt};
use itertools::Either;
use thiserror::Error;

use crate::{
    manifest::mk_manifests_table, structure::mk_structure_table, AddNodeError,
    ArrayIndices, ChangeSet, ChunkInfo, ChunkPayload, Dataset, Flags, ManifestExtents,
    ManifestRef, NodeData, NodeId, NodeStructure, ObjectId, Path, Storage, StorageError,
    TableRegion, UpdateNodeError, UserAttributes, UserAttributesStructure,
    ZarrArrayMetadata,
};

impl ChangeSet {
    fn add_group(&mut self, path: Path, node_id: NodeId) {
        self.new_groups.insert(path, node_id);
    }

    fn get_group(&self, path: &Path) -> Option<&NodeId> {
        self.new_groups.get(path)
    }

    fn add_array(&mut self, path: Path, node_id: NodeId, metadata: ZarrArrayMetadata) {
        self.new_arrays.insert(path, (node_id, metadata));
    }

    fn get_array(&self, path: &Path) -> Option<&(NodeId, ZarrArrayMetadata)> {
        self.new_arrays.get(path)
    }

    fn update_array(&mut self, path: Path, metadata: ZarrArrayMetadata) {
        self.updated_arrays.insert(path, metadata);
    }

    fn get_updated_zarr_metadata(&self, path: &Path) -> Option<&ZarrArrayMetadata> {
        self.updated_arrays.get(path)
    }

    fn update_user_attributes(&mut self, path: Path, atts: Option<UserAttributes>) {
        self.updated_attributes.insert(path, atts);
    }

    fn get_user_attributes(&self, path: &Path) -> Option<&Option<UserAttributes>> {
        self.updated_attributes.get(path)
    }

    fn set_chunk(&mut self, path: Path, coord: ArrayIndices, data: Option<ChunkPayload>) {
        self.set_chunks
            .entry(path)
            .and_modify(|h| {
                h.insert(coord.clone(), data.clone());
            })
            .or_insert(HashMap::from([(coord, data)]));
    }

    fn get_chunk_ref(
        &self,
        path: &Path,
        coords: &ArrayIndices,
    ) -> Option<&Option<ChunkPayload>> {
        self.set_chunks.get(path).and_then(|h| h.get(coords))
    }

    fn array_chunks_iterator(
        &self,
        path: &Path,
    ) -> impl Iterator<Item = (&ArrayIndices, &Option<ChunkPayload>)> {
        match self.set_chunks.get(path) {
            None => Either::Left(iter::empty()),
            Some(h) => Either::Right(h.iter()),
        }
    }

    fn new_arrays_chunk_iterator(&self) -> impl Iterator<Item = ChunkInfo> + '_ {
        self.new_arrays.iter().flat_map(|(path, (node_id, _))| {
            self.array_chunks_iterator(path).filter_map(|(coords, payload)| {
                payload.as_ref().map(|p| ChunkInfo {
                    node: *node_id,
                    coord: coords.clone(),
                    payload: p.clone(),
                })
            })
        })
    }

    fn new_nodes(&self) -> impl Iterator<Item = &Path> {
        self.new_groups.keys().chain(self.new_arrays.keys())
    }
}
/// FIXME: what do we want to do with implicit groups?
///
impl Dataset {
    pub fn create(storage: Arc<dyn Storage>) -> Self {
        Dataset::new(storage, None)
    }

    // FIXME: the ObjectIds should include a type of object to avoid mistakes at compile time
    pub fn update(
        storage: Arc<dyn Storage>,
        previous_version_structure_id: ObjectId,
    ) -> Self {
        Dataset::new(storage, Some(previous_version_structure_id))
    }

    fn new(
        storage: Arc<dyn Storage>,
        previous_version_structure_id: Option<ObjectId>,
    ) -> Self {
        Dataset {
            structure_id: previous_version_structure_id,
            storage,
            last_node_id: None,
            change_set: ChangeSet::default(),
        }
    }

    /// Add a group to the store.
    ///
    /// Calling this only records the operation in memory, doesn't have any consequence on the storage
    pub async fn add_group(&mut self, path: Path) -> Result<(), AddNodeError> {
        if self.get_node(&path).await.is_none() {
            let id = self.reserve_node_id().await;
            self.change_set.add_group(path, id);
            Ok(())
        } else {
            Err(AddNodeError::AlreadyExists(path))
        }
    }

    /// Add an array to the store.
    ///
    /// Calling this only records the operation in memory, doesn't have any consequence on the storage
    pub async fn add_array(
        &mut self,
        path: Path,
        metadata: ZarrArrayMetadata,
    ) -> Result<(), AddNodeError> {
        if self.get_node(&path).await.is_none() {
            let id = self.reserve_node_id().await;
            self.change_set.add_array(path, id, metadata);
            Ok(())
        } else {
            Err(AddNodeError::AlreadyExists(path))
        }
    }

    // Updates an array Zarr metadata
    ///
    /// Calling this only records the operation in memory, doesn't have any consequence on the storage
    pub async fn update_array(
        &mut self,
        path: Path,
        metadata: ZarrArrayMetadata,
    ) -> Result<(), UpdateNodeError> {
        match self.get_node(&path).await {
            None => Err(UpdateNodeError::NotFound(path)),
            Some(NodeStructure { node_data: NodeData::Array(..), .. }) => {
                self.change_set.update_array(path, metadata);
                Ok(())
            }
            Some(_) => Err(UpdateNodeError::NotAnArray(path)),
        }
    }

    /// Record the write or delete of user attributes to array or group
    pub async fn set_user_attributes(
        &mut self,
        path: Path,
        atts: Option<UserAttributes>,
    ) -> Result<(), UpdateNodeError> {
        match self.get_node(&path).await {
            None => Err(UpdateNodeError::NotFound(path)),
            Some(_) => {
                self.change_set.update_user_attributes(path, atts);
                Ok(())
            }
        }
    }

    // Record the write, referenceing or delete of a chunk
    //
    // Caller has to write the chunk before calling this.
    pub async fn set_chunk(
        &mut self,
        path: Path,
        coord: ArrayIndices,
        data: Option<ChunkPayload>,
    ) -> Result<(), UpdateNodeError> {
        match self.get_node(&path).await {
            None => Err(UpdateNodeError::NotFound(path)),
            Some(NodeStructure { node_data: NodeData::Array(..), .. }) => {
                self.change_set.set_chunk(path, coord, data);
                Ok(())
            }
            Some(_) => Err(UpdateNodeError::NotAnArray(path)),
        }
    }

    async fn compute_last_node_id(&self) -> NodeId {
        // FIXME: errors
        match &self.structure_id {
            None => 0,
            Some(id) => self
                .storage
                .fetch_structure(id)
                .await
                // FIXME: bubble up the error
                .ok()
                .and_then(|structure| structure.iter().max_by_key(|s| s.id))
                .map_or(0, |node| node.id),
        }
    }

    async fn reserve_node_id(&mut self) -> NodeId {
        let last = self.last_node_id.unwrap_or(self.compute_last_node_id().await);
        let new = last + 1;
        self.last_node_id = Some(new);
        new
    }

    // FIXME: add list, deletes, moves

    // FIXME: we should have errros here, not only None
    pub async fn get_node(&self, path: &Path) -> Option<NodeStructure> {
        self.get_new_node(path).or(self.get_existing_node(path).await)
    }

    async fn get_existing_node(&self, path: &Path) -> Option<NodeStructure> {
        let structure_id = self.structure_id.as_ref()?;
        let structure = self.storage.fetch_structure(structure_id).await.ok()?;
        let session_atts = self
            .change_set
            .get_user_attributes(path)
            .cloned()
            .map(|a| a.map(UserAttributesStructure::Inline));
        let res = structure.get_node(path)?;
        let res = NodeStructure {
            user_attributes: session_atts.unwrap_or(res.user_attributes),
            ..res
        };
        if let Some(session_meta) =
            self.change_set.get_updated_zarr_metadata(path).cloned()
        {
            if let NodeData::Array(_, manifests) = res.node_data {
                Some(NodeStructure {
                    node_data: NodeData::Array(session_meta, manifests),
                    ..res
                })
            } else {
                Some(res)
            }
        } else {
            Some(res)
        }
    }

    fn get_new_node(&self, path: &Path) -> Option<NodeStructure> {
        self.get_new_array(path).or(self.get_new_group(path))
    }

    fn get_new_array(&self, path: &Path) -> Option<NodeStructure> {
        self.change_set.get_array(path).map(|(id, meta)| {
            let meta =
                self.change_set.get_updated_zarr_metadata(path).unwrap_or(meta).clone();
            let atts = self.change_set.get_user_attributes(path).cloned();
            NodeStructure {
                id: *id,
                path: path.clone(),
                user_attributes: atts.flatten().map(UserAttributesStructure::Inline),
                // We put no manifests in new arrays, see get_chunk_ref to understand how chunks get
                // fetched for those arrays
                node_data: NodeData::Array(meta.clone(), vec![]),
            }
        })
    }

    fn get_new_group(&self, path: &Path) -> Option<NodeStructure> {
        self.change_set.get_group(path).map(|id| {
            let atts = self.change_set.get_user_attributes(path).cloned();
            NodeStructure {
                id: *id,
                path: path.clone(),
                user_attributes: atts.flatten().map(UserAttributesStructure::Inline),
                node_data: NodeData::Group,
            }
        })
    }

    pub async fn get_chunk_ref(
        &self,
        path: &Path,
        coords: &ArrayIndices,
    ) -> Option<ChunkPayload> {
        // FIXME: better error type
        let node = self.get_node(path).await?;
        match node.node_data {
            NodeData::Group => None,
            NodeData::Array(_, manifests) => {
                // check the chunks modified in this session first
                // TODO: I hate rust forces me to clone to search in a hashmap. How to do better?
                let session_chunk = self.change_set.get_chunk_ref(path, coords).cloned();
                // If session_chunk is not None we have to return it, because is the update the
                // user made in the current session
                // If session_chunk == None, user hasn't modified the chunk in this session and we
                // need to fallback to fetching the manifests
                session_chunk
                    .unwrap_or(self.get_old_chunk(manifests.as_slice(), coords).await)
            }
        }
    }

    async fn get_old_chunk(
        &self,
        manifests: &[ManifestRef],
        coords: &ArrayIndices,
    ) -> Option<ChunkPayload> {
        // FIXME: use manifest extents
        for manifest in manifests {
            let manifest_structure =
                self.storage.fetch_manifests(&manifest.object_id).await.ok()?;
            if let Some(payload) = manifest_structure
                .get_chunk_info(coords, &manifest.location)
                .map(|info| info.payload)
            {
                return Some(payload);
            }
        }
        None
    }

    async fn updated_chunk_iterator(&self) -> impl Stream<Item = ChunkInfo> + '_ {
        match self.structure_id.as_ref() {
            None => futures::future::Either::Left(futures::stream::empty()),
            Some(structure_id) => {
                // FIXME: error handling
                let structure = self.storage.fetch_structure(structure_id).await.unwrap();
                let nodes = futures::stream::iter(structure.iter_arc());
                futures::future::Either::Right(
                    nodes
                        .then(move |node| async move {
                            self.node_chunk_iterator(node).await
                        })
                        .flatten(),
                )
            }
        }
    }

    async fn node_chunk_iterator(
        &self,
        node: NodeStructure,
    ) -> impl Stream<Item = ChunkInfo> + '_ {
        match node.node_data {
            NodeData::Group => futures::future::Either::Left(futures::stream::empty()),
            NodeData::Array(_, manifests) => futures::future::Either::Right(
                futures::stream::iter(manifests)
                    .then(move |manifest_ref| {
                        let path = node.path.clone();
                        let node_id = node.id;
                        async move {
                            let manifest = self
                                .storage
                                .fetch_manifests(&manifest_ref.object_id)
                                .await
                                .unwrap();

                            let new_chunk_indices: HashSet<&ArrayIndices> = self
                                .change_set
                                .array_chunks_iterator(&path)
                                .map(|(idx, _)| idx)
                                .collect();

                            let new_chunks = self
                                .change_set
                                .array_chunks_iterator(&path)
                                .filter_map(move |(idx, payload)| {
                                    payload.as_ref().map(|payload| ChunkInfo {
                                        node: node_id,
                                        coord: idx.clone(),
                                        payload: payload.clone(),
                                    })
                                });

                            let old_chunks = manifest
                                .iter(
                                    Some(manifest_ref.location.0),
                                    Some(manifest_ref.location.1),
                                )
                                .filter(move |c| !new_chunk_indices.contains(&c.coord));

                            let old_chunks =
                                self.update_existing_chunks(path.clone(), old_chunks);
                            //FIXME: error handling
                            futures::stream::iter(new_chunks.chain(old_chunks))
                        }
                    })
                    .flatten(),
            ),
        }
    }

    fn update_existing_chunks<'a>(
        &'a self,
        path: Path,
        chunks: impl Iterator<Item = ChunkInfo> + 'a,
    ) -> impl Iterator<Item = ChunkInfo> + 'a {
        chunks.filter_map(move |chunk| {
            match self.change_set.get_chunk_ref(&path, &chunk.coord) {
                None => Some(chunk),
                Some(new_payload) => {
                    new_payload.clone().map(|pl| ChunkInfo { payload: pl, ..chunk })
                }
            }
        })
    }

    async fn updated_existing_nodes<'a>(
        &'a self,
        manifest_id: &'a ObjectId,
        manifest_tracker: &'a TableRegionTracker,
    ) -> impl Iterator<Item = NodeStructure> + 'a {
        // TODO: solve this duplication, there is always the possibility of this being the first
        // version
        match &self.structure_id {
            None => Either::Left(iter::empty()),
            Some(id) => Either::Right(
                self.storage
                    .fetch_structure(id)
                    .await
                    // FIXME: bubble up the error
                    .unwrap()
                    .iter_arc()
                    .map(|node| {
                        let region = manifest_tracker.region(node.id);
                        let new_manifests = region.map(|r| {
                            if r.0 == r.1 {
                                vec![]
                            } else {
                                vec![ManifestRef {
                                    object_id: manifest_id.clone(),
                                    location: r.clone(),
                                    flags: Flags(),
                                    extents: ManifestExtents(vec![]),
                                }]
                            }
                        });
                        self.update_existing_node(node, new_manifests)
                    }),
            ),
        }
    }

    fn new_nodes<'a>(
        &'a self,
        manifest_id: &'a ObjectId,
        manifest_tracker: &'a TableRegionTracker,
    ) -> impl Iterator<Item = NodeStructure> + 'a {
        // FIXME: unwrap
        self.change_set.new_nodes().map(|path| {
            let node = self.get_new_node(path).unwrap();
            match node.node_data {
                NodeData::Group => node,
                NodeData::Array(meta, _no_manifests_yet) => {
                    let region = manifest_tracker.region(node.id);
                    let new_manifests = region.map(|r| {
                        if r.0 == r.1 {
                            vec![]
                        } else {
                            vec![ManifestRef {
                                object_id: manifest_id.clone(),
                                location: r.clone(),
                                flags: Flags(),
                                extents: ManifestExtents(vec![]),
                            }]
                        }
                    });
                    NodeStructure {
                        node_data: NodeData::Array(
                            meta,
                            new_manifests.unwrap_or_default(),
                        ),
                        ..node
                    }
                }
            }
        })
    }

    async fn updated_nodes<'a>(
        &'a self,
        manifest_id: &'a ObjectId,
        manifest_tracker: &'a TableRegionTracker,
    ) -> impl Iterator<Item = NodeStructure> + 'a {
        self.updated_existing_nodes(manifest_id, manifest_tracker)
            .await
            .chain(self.new_nodes(manifest_id, manifest_tracker))
    }

    fn update_existing_node(
        &self,
        node: NodeStructure,
        new_manifests: Option<Vec<ManifestRef>>,
    ) -> NodeStructure {
        let session_atts = self
            .change_set
            .get_user_attributes(&node.path)
            .cloned()
            .map(|a| a.map(UserAttributesStructure::Inline));
        let new_atts = session_atts.unwrap_or(node.user_attributes);
        match node.node_data {
            NodeData::Group => NodeStructure { user_attributes: new_atts, ..node },
            NodeData::Array(old_zarr_meta, _) => {
                let new_zarr_meta = self
                    .change_set
                    .get_updated_zarr_metadata(&node.path)
                    .cloned()
                    .unwrap_or(old_zarr_meta);

                NodeStructure {
                    // FIXME: bad option type, change
                    node_data: NodeData::Array(
                        new_zarr_meta,
                        new_manifests.unwrap_or_default(),
                    ),
                    user_attributes: new_atts,
                    ..node
                }
            }
        }
    }

    /// After changes to the dasate have been made, this generates and writes to `Storage` the updated datastructures.
    ///
    /// After calling this, changes are reset and the [Dataset] can continue to be used for further
    /// changes.
    ///
    /// Returns the `ObjectId` of the new structure file. It's the callers responsibility to commit
    /// this id change.
    pub async fn flush(&mut self) -> Result<ObjectId, FlushError> {
        let mut region_tracker = TableRegionTracker::default();
        let existing_array_chunks = self.updated_chunk_iterator().await;
        let new_array_chunks =
            futures::stream::iter(self.change_set.new_arrays_chunk_iterator());
        let all_chunks = existing_array_chunks.chain(new_array_chunks).map(|chunk| {
            region_tracker.update(&chunk);
            chunk
        });
        let new_manifest = mk_manifests_table(all_chunks).await;
        let new_manifest_id = ObjectId::random();
        self.storage
            .write_manifests(new_manifest_id.clone(), Arc::new(new_manifest))
            .await?;

        let all_nodes = self.updated_nodes(&new_manifest_id, &region_tracker).await;
        let new_structure = mk_structure_table(all_nodes);
        let new_structure_id = ObjectId::random();
        self.storage
            .write_structure(new_structure_id.clone(), Arc::new(new_structure))
            .await?;

        self.structure_id = Some(new_structure_id.clone());
        self.change_set = ChangeSet::default();
        Ok(new_structure_id)
    }
}

#[derive(Debug, Clone, Default)]
struct TableRegionTracker(HashMap<NodeId, TableRegion>, u32);

impl TableRegionTracker {
    fn update(&mut self, chunk: &ChunkInfo) {
        self.0
            .entry(chunk.node)
            .and_modify(|tr| tr.1 = self.1 + 1)
            .or_insert(TableRegion(self.1, self.1 + 1));
        self.1 += 1;
    }

    fn region(&self, node: NodeId) -> Option<&TableRegion> {
        self.0.get(&node)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FlushError {
    #[error("no changes made to the dataset")]
    NoChangesToFlush,
    #[error("error contacting storage")]
    StorageError(#[from] StorageError),
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, error::Error, num::NonZeroU64, path::PathBuf};

    use crate::{
        manifest::mk_manifests_table, storage::InMemoryStorage,
        structure::mk_structure_table, ChunkInfo, ChunkKeyEncoding, ChunkRef, ChunkShape,
        Codecs, DataType, FillValue, Flags, ManifestExtents, StorageTransformers,
        TableRegion,
    };

    use super::*;
    use pretty_assertions::assert_eq;

    #[tokio::test(flavor = "multi_thread")]
    async fn test_dataset_with_updates() -> Result<(), Box<dyn Error>> {
        let storage = InMemoryStorage::new();

        let array_id = 2;
        let chunk1 = ChunkInfo {
            node: array_id,
            coord: ArrayIndices(vec![0, 0, 0]),
            payload: ChunkPayload::Ref(ChunkRef {
                id: ObjectId::random(),
                offset: 0,
                length: 4,
            }),
        };

        let chunk2 = ChunkInfo {
            node: array_id,
            coord: ArrayIndices(vec![0, 0, 1]),
            payload: ChunkPayload::Inline(vec![0, 0, 0, 42]),
        };

        let manifest = Arc::new(
            mk_manifests_table(futures::stream::iter(vec![
                chunk1.clone(),
                chunk2.clone(),
            ]))
            .await,
        );
        let manifest_id = ObjectId::random();
        storage.write_manifests(manifest_id.clone(), manifest).await?;

        let zarr_meta1 = ZarrArrayMetadata {
            shape: vec![2, 2, 2],
            data_type: DataType::Int32,
            chunk_shape: ChunkShape(vec![
                NonZeroU64::new(1).unwrap(),
                NonZeroU64::new(1).unwrap(),
                NonZeroU64::new(1).unwrap(),
            ]),
            chunk_key_encoding: ChunkKeyEncoding::Slash,
            fill_value: FillValue::Int32(0),
            codecs: Codecs("codec".to_string()),
            storage_transformers: Some(StorageTransformers("tranformers".to_string())),
            dimension_names: Some(vec![
                Some("x".to_string()),
                Some("y".to_string()),
                Some("t".to_string()),
            ]),
        };
        let manifest_ref = ManifestRef {
            object_id: manifest_id,
            location: TableRegion(0, 2),
            flags: Flags(),
            extents: ManifestExtents(vec![]),
        };
        let array1_path: PathBuf = "/array1".to_string().into();
        let nodes = vec![
            NodeStructure {
                path: "/".into(),
                id: 1,
                user_attributes: None,
                node_data: NodeData::Group,
            },
            NodeStructure {
                path: array1_path.clone(),
                id: array_id,
                user_attributes: Some(UserAttributesStructure::Inline(
                    "{foo:1}".to_string(),
                )),
                node_data: NodeData::Array(zarr_meta1.clone(), vec![manifest_ref]),
            },
        ];

        let structure = Arc::new(mk_structure_table(nodes.clone()));
        let structure_id = ObjectId::random();
        storage.write_structure(structure_id.clone(), structure).await?;
        let mut ds = Dataset::update(Arc::new(storage), structure_id);

        // retrieve the old array node
        let node = ds.get_node(&array1_path).await;
        assert_eq!(nodes.get(1), node.as_ref());

        // add a new array and retrieve its node
        ds.add_group("/group".to_string().into()).await?;

        let zarr_meta2 = ZarrArrayMetadata {
            shape: vec![3],
            data_type: DataType::Int32,
            chunk_shape: ChunkShape(vec![NonZeroU64::new(2).unwrap()]),
            chunk_key_encoding: ChunkKeyEncoding::Slash,
            fill_value: FillValue::Int32(0),
            codecs: Codecs("codec".to_string()),
            storage_transformers: Some(StorageTransformers("tranformers".to_string())),
            dimension_names: Some(vec![Some("t".to_string())]),
        };

        let new_array_path: PathBuf = "/group/array2".to_string().into();
        ds.add_array(new_array_path.clone(), zarr_meta2.clone()).await?;

        let node = ds.get_node(&new_array_path).await;
        assert_eq!(
            node,
            Some(NodeStructure {
                path: new_array_path.clone(),
                id: 4,
                user_attributes: None,
                node_data: NodeData::Array(zarr_meta2.clone(), vec![]),
            })
        );

        // set user attributes for the new array and retrieve them
        ds.set_user_attributes(new_array_path.clone(), Some("{n:42}".to_string()))
            .await?;
        let node = ds.get_node(&new_array_path).await;
        assert_eq!(
            node,
            Some(NodeStructure {
                path: "/group/array2".into(),
                id: 4,
                user_attributes: Some(UserAttributesStructure::Inline(
                    "{n:42}".to_string(),
                )),
                node_data: NodeData::Array(zarr_meta2.clone(), vec![]),
            })
        );

        // set a chunk for the new array and  retrieve it
        ds.set_chunk(
            new_array_path.clone(),
            ArrayIndices(vec![0]),
            Some(ChunkPayload::Inline(vec![0, 0, 0, 7])),
        )
        .await?;

        let chunk = ds.get_chunk_ref(&new_array_path, &ArrayIndices(vec![0])).await;
        assert_eq!(chunk, Some(ChunkPayload::Inline(vec![0, 0, 0, 7])));

        // retrieve a non initialized chunk of the new array
        let non_chunk = ds.get_chunk_ref(&new_array_path, &ArrayIndices(vec![1])).await;
        assert_eq!(non_chunk, None);

        // update old array use attriutes and check them
        ds.set_user_attributes(array1_path.clone(), Some("{updated: true}".to_string()))
            .await?;
        let node = ds.get_node(&array1_path).await.unwrap();
        assert_eq!(
            node.user_attributes,
            Some(UserAttributesStructure::Inline("{updated: true}".to_string()))
        );

        // update old array zarr metadata and check it
        let new_zarr_meta1 = ZarrArrayMetadata { shape: vec![2, 2, 3], ..zarr_meta1 };
        ds.update_array(array1_path.clone(), new_zarr_meta1).await?;
        let node = ds.get_node(&array1_path).await;
        if let Some(NodeStructure {
            node_data: NodeData::Array(ZarrArrayMetadata { shape, .. }, _),
            ..
        }) = node
        {
            assert_eq!(shape, vec![2, 2, 3]);
        } else {
            panic!("Failed to update zarr metadata");
        }

        // set old array chunk and check them
        ds.set_chunk(
            array1_path.clone(),
            ArrayIndices(vec![0, 0, 0]),
            Some(ChunkPayload::Inline(vec![0, 0, 0, 99])),
        )
        .await?;

        let chunk = ds.get_chunk_ref(&array1_path, &ArrayIndices(vec![0, 0, 0])).await;
        assert_eq!(chunk, Some(ChunkPayload::Inline(vec![0, 0, 0, 99])));

        Ok(())
    }

    #[test]
    fn test_new_arrays_chunk_iterator() {
        let mut change_set = ChangeSet::default();
        assert_eq!(None, change_set.new_arrays_chunk_iterator().next());

        let zarr_meta = ZarrArrayMetadata {
            shape: vec![2, 2, 2],
            data_type: DataType::Int32,
            chunk_shape: ChunkShape(vec![
                NonZeroU64::new(1).unwrap(),
                NonZeroU64::new(1).unwrap(),
                NonZeroU64::new(1).unwrap(),
            ]),
            chunk_key_encoding: ChunkKeyEncoding::Slash,
            fill_value: FillValue::Int32(0),
            codecs: Codecs("codec".to_string()),
            storage_transformers: Some(StorageTransformers("tranformers".to_string())),
            dimension_names: Some(vec![
                Some("x".to_string()),
                Some("y".to_string()),
                Some("t".to_string()),
            ]),
        };

        change_set.add_array("foo/bar".into(), 1, zarr_meta.clone());
        change_set.add_array("foo/baz".into(), 2, zarr_meta);
        assert_eq!(None, change_set.new_arrays_chunk_iterator().next());

        change_set.set_chunk("foo/bar".into(), ArrayIndices(vec![0, 1]), None);
        assert_eq!(None, change_set.new_arrays_chunk_iterator().next());

        change_set.set_chunk(
            "foo/bar".into(),
            ArrayIndices(vec![1, 0]),
            Some(ChunkPayload::Inline(b"bar1".into())),
        );
        change_set.set_chunk(
            "foo/bar".into(),
            ArrayIndices(vec![1, 1]),
            Some(ChunkPayload::Inline(b"bar2".into())),
        );
        change_set.set_chunk(
            "foo/baz".into(),
            ArrayIndices(vec![0]),
            Some(ChunkPayload::Inline(b"baz1".into())),
        );
        change_set.set_chunk(
            "foo/baz".into(),
            ArrayIndices(vec![1]),
            Some(ChunkPayload::Inline(b"baz2".into())),
        );

        {
            let all_chunks: HashSet<_> = change_set.new_arrays_chunk_iterator().collect();
            let expected_chunks = [
                ChunkInfo {
                    node: 1,
                    coord: ArrayIndices(vec![1, 0]),
                    payload: ChunkPayload::Inline(b"bar1".into()),
                },
                ChunkInfo {
                    node: 1,
                    coord: ArrayIndices(vec![1, 1]),
                    payload: ChunkPayload::Inline(b"bar2".into()),
                },
                ChunkInfo {
                    node: 2,
                    coord: ArrayIndices(vec![0]),
                    payload: ChunkPayload::Inline(b"baz1".into()),
                },
                ChunkInfo {
                    node: 2,
                    coord: ArrayIndices(vec![1]),
                    payload: ChunkPayload::Inline(b"baz2".into()),
                },
            ]
            .into();
            assert_eq!(all_chunks, expected_chunks);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_dataset_with_updates_and_writes() -> Result<(), Box<dyn Error>> {
        let storage: Arc<dyn Storage> = Arc::new(InMemoryStorage::new());
        let mut ds = Dataset::create(Arc::clone(&storage));

        // add a new array and retrieve its node
        ds.add_group("/".into()).await?;
        let structure_id = ds.flush().await?;

        assert_eq!(Some(structure_id), ds.structure_id);
        assert_eq!(
            ds.get_node(&"/".into()).await,
            Some(NodeStructure {
                id: 1,
                path: "/".into(),
                user_attributes: None,
                node_data: NodeData::Group
            })
        );
        ds.add_group("/group".into()).await?;
        let _structure_id = ds.flush().await?;
        assert_eq!(
            ds.get_node(&"/".into()).await,
            Some(NodeStructure {
                id: 1,
                path: "/".into(),
                user_attributes: None,
                node_data: NodeData::Group
            })
        );
        assert_eq!(
            ds.get_node(&"/group".into()).await,
            Some(NodeStructure {
                id: 2,
                path: "/group".into(),
                user_attributes: None,
                node_data: NodeData::Group
            })
        );
        let zarr_meta = ZarrArrayMetadata {
            shape: vec![1, 1, 2],
            data_type: DataType::Int32,
            chunk_shape: ChunkShape(vec![NonZeroU64::new(2).unwrap()]),
            chunk_key_encoding: ChunkKeyEncoding::Slash,
            fill_value: FillValue::Int32(0),
            codecs: Codecs("codec".to_string()),
            storage_transformers: Some(StorageTransformers("tranformers".to_string())),
            dimension_names: Some(vec![Some("t".to_string())]),
        };

        let new_array_path: PathBuf = "/group/array1".to_string().into();
        ds.add_array(new_array_path.clone(), zarr_meta.clone()).await?;

        // we set a chunk in a new array
        ds.set_chunk(
            new_array_path.clone(),
            ArrayIndices(vec![0, 0, 0]),
            Some(ChunkPayload::Inline(b"hello".into())),
        )
        .await?;

        let _structure_id = ds.flush().await?;
        assert_eq!(
            ds.get_node(&"/".into()).await,
            Some(NodeStructure {
                id: 1,
                path: "/".into(),
                user_attributes: None,
                node_data: NodeData::Group
            })
        );
        assert_eq!(
            ds.get_node(&"/group".into()).await,
            Some(NodeStructure {
                id: 2,
                path: "/group".into(),
                user_attributes: None,
                node_data: NodeData::Group
            })
        );
        assert!(matches!(
            ds.get_node(&new_array_path).await,
            Some(NodeStructure {
                id: 3,
                path,
                user_attributes: None,
                node_data: NodeData::Array(meta, manifests)
            }) if path == new_array_path && meta == zarr_meta.clone() && manifests.len() == 1
        ));
        assert_eq!(
            ds.get_chunk_ref(&new_array_path, &ArrayIndices(vec![0, 0, 0])).await,
            Some(ChunkPayload::Inline(b"hello".into()))
        );

        // we modify a chunk in an existing array
        ds.set_chunk(
            new_array_path.clone(),
            ArrayIndices(vec![0, 0, 0]),
            Some(ChunkPayload::Inline(b"bye".into())),
        )
        .await?;

        // we add a new chunk in an existing array
        ds.set_chunk(
            new_array_path.clone(),
            ArrayIndices(vec![0, 0, 1]),
            Some(ChunkPayload::Inline(b"new chunk".into())),
        )
        .await?;

        let previous_structure_id = ds.flush().await?;
        assert_eq!(
            ds.get_chunk_ref(&new_array_path, &ArrayIndices(vec![0, 0, 0])).await,
            Some(ChunkPayload::Inline(b"bye".into()))
        );
        assert_eq!(
            ds.get_chunk_ref(&new_array_path, &ArrayIndices(vec![0, 0, 1])).await,
            Some(ChunkPayload::Inline(b"new chunk".into()))
        );

        // we delete a chunk
        ds.set_chunk(new_array_path.clone(), ArrayIndices(vec![0, 0, 1]), None).await?;

        let new_meta = ZarrArrayMetadata { shape: vec![1, 1, 1], ..zarr_meta };
        // we change zarr metadata
        ds.update_array(new_array_path.clone(), new_meta.clone()).await?;

        // we change user attributes metadata
        ds.set_user_attributes(new_array_path.clone(), Some("{foo:42}".to_string()))
            .await?;

        let structure_id = ds.flush().await?;
        let ds = Dataset::update(Arc::clone(&storage), structure_id);

        assert_eq!(
            ds.get_chunk_ref(&new_array_path, &ArrayIndices(vec![0, 0, 0])).await,
            Some(ChunkPayload::Inline(b"bye".into()))
        );
        assert_eq!(
            ds.get_chunk_ref(&new_array_path, &ArrayIndices(vec![0, 0, 1])).await,
            None
        );
        assert!(matches!(
            ds.get_node(&new_array_path).await,
            Some(NodeStructure {
                id: 3,
                path,
                user_attributes: Some(atts),
                node_data: NodeData::Array(meta, manifests)
            }) if path == new_array_path && meta == new_meta.clone() && manifests.len() == 1 && atts == UserAttributesStructure::Inline("{foo:42}".to_string())
        ));

        //test the previous version is still alive
        let ds = Dataset::update(Arc::clone(&storage), previous_structure_id);
        assert_eq!(
            ds.get_chunk_ref(&new_array_path, &ArrayIndices(vec![0, 0, 0])).await,
            Some(ChunkPayload::Inline(b"bye".into()))
        );
        assert_eq!(
            ds.get_chunk_ref(&new_array_path, &ArrayIndices(vec![0, 0, 1])).await,
            Some(ChunkPayload::Inline(b"new chunk".into()))
        );

        Ok(())
    }
}