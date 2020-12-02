use graph::{
    data::subgraph::schema::MetadataType,
    data::subgraph::schema::SubgraphDeploymentAssignmentEntity,
    data::subgraph::schema::SubgraphEntity,
    data::subgraph::schema::SUBGRAPHS_ID,
    prelude::EntityChange,
    prelude::EntityChangeOperation,
    prelude::EntityKey,
    prelude::Schema,
    prelude::StoreEvent,
    prelude::SubgraphDeploymentEntity,
    prelude::SubgraphManifest,
    prelude::SubgraphName,
    prelude::SubgraphVersionSwitchingMode,
    prelude::TypedEntity,
    prelude::{NodeId, Store as _, SubgraphDeploymentId},
};
use graph_store_postgres::NetworkStore;
use std::collections::HashSet;
use test_store::*;

const SUBGRAPH_GQL: &str = "
    type User @entity {
        id: ID!,
        name: String
    }
";

fn set(typ: MetadataType, id: &str) -> EntityChange {
    EntityChange {
        subgraph_id: SUBGRAPHS_ID.clone(),
        entity_type: typ.to_string(),
        entity_id: id.to_string(),
        operation: EntityChangeOperation::Set,
    }
}

fn removed(typ: MetadataType, id: &str) -> EntityChange {
    EntityChange {
        subgraph_id: SUBGRAPHS_ID.clone(),
        entity_type: typ.to_string(),
        entity_id: id.to_string(),
        operation: EntityChangeOperation::Removed,
    }
}

#[test]
fn reassign_subgraph() {
    fn setup() -> SubgraphDeploymentId {
        let id = SubgraphDeploymentId::new("reassignSubgraph").unwrap();
        remove_subgraphs();
        create_test_subgraph(&id, SUBGRAPH_GQL);
        id
    }

    fn find_assignment(store: &NetworkStore, id: &SubgraphDeploymentId) -> Option<String> {
        store
            .get(SubgraphDeploymentAssignmentEntity::key(id.clone()))
            .unwrap()
            .and_then(|entity| entity.get("nodeId").cloned())
            .and_then(|value| value.as_string())
    }

    run_test_sequentially(setup, |store, id| async move {
        // Check our setup
        let node = find_assignment(store.as_ref(), &id);
        assert_eq!(Some("test".to_string()), node);

        // Assign to node 'left' twice, the first time we assign from 'test'
        // to 'left', the second time from 'left' to 'left', with the same results
        for _ in 0..2 {
            let node = NodeId::new("left").unwrap();
            let expected = vec![StoreEvent::new(vec![set(
                MetadataType::SubgraphDeploymentAssignment,
                id.as_str(),
            )])];

            let events = tap_store_events(|| store.reassign_subgraph(&id, &node).unwrap());
            let node = find_assignment(store.as_ref(), &id);
            assert_eq!(Some("left"), node.as_deref());
            assert_eq!(expected, events);
        }
    })
}

#[test]
fn create_subgraph() {
    const SUBGRAPH_NAME: &str = "create/subgraph";

    /// Get the deployment for the given `SubgraphVersion`
    fn deployment_from_version(store: &NetworkStore, version: String) -> String {
        let entity = store
            .get(EntityKey {
                subgraph_id: SUBGRAPHS_ID.clone(),
                entity_type: "SubgraphVersion".to_string(),
                entity_id: version,
            })
            .unwrap()
            .unwrap();
        entity.get("deployment").unwrap().to_string()
    }

    // Return the versions (not deployments) for a subgraph
    fn subgraph_versions(
        store: &NetworkStore,
        entity_id: &str,
    ) -> (Option<String>, Option<String>) {
        let entity = store
            .get(EntityKey {
                subgraph_id: SUBGRAPHS_ID.clone(),
                entity_type: SubgraphEntity::TYPENAME.to_string(),
                entity_id: entity_id.to_string(),
            })
            .unwrap()
            .unwrap();

        (
            entity.get("currentVersion").map(|v| v.to_string()),
            entity.get("pendingVersion").map(|v| v.to_string()),
        )
    }

    /// Return the deployment for the current and the pending version of the
    /// subgraph with the given `entity_id`
    fn subgraph_deployments(
        store: &NetworkStore,
        entity_id: &str,
    ) -> (Option<String>, Option<String>) {
        let (current, pending) = subgraph_versions(store, entity_id);
        (
            current.map(|v| deployment_from_version(store, v)),
            pending.map(|v| deployment_from_version(store, v)),
        )
    }

    fn deploy(
        store: &NetworkStore,
        id: &str,
        mode: SubgraphVersionSwitchingMode,
    ) -> HashSet<EntityChange> {
        let name = SubgraphName::new(SUBGRAPH_NAME.to_string()).unwrap();
        let id = SubgraphDeploymentId::new(id.to_string()).unwrap();
        let schema = Schema::parse(SUBGRAPH_GQL, id.clone()).unwrap();

        let manifest = SubgraphManifest {
            id: id.clone(),
            location: String::new(),
            spec_version: "1".to_owned(),
            description: None,
            repository: None,
            schema: schema.clone(),
            data_sources: vec![],
            graft: None,
            templates: vec![],
        };
        let deployment = SubgraphDeploymentEntity::new(&manifest, false, None);
        let node_id = NodeId::new("left").unwrap();

        tap_store_events(|| {
            store
                .create_subgraph_deployment(name, &schema, deployment, node_id, mode)
                .unwrap()
        })
        .into_iter()
        .map(|event| event.changes.into_iter())
        .flatten()
        .collect()
    }

    fn deploy_event(id: &str) -> HashSet<EntityChange> {
        let mut changes = HashSet::new();
        changes.insert(set(MetadataType::SubgraphDeployment, id));
        changes.insert(set(MetadataType::SubgraphDeploymentAssignment, id));
        changes.insert(set(
            MetadataType::SubgraphManifest,
            &format!("{}-manifest", id),
        ));
        changes
    }

    // Test VersionSwitchingMode::Instant
    run_test_sequentially(remove_subgraphs, |store, _| async move {
        const MODE: SubgraphVersionSwitchingMode = SubgraphVersionSwitchingMode::Instant;
        const ID: &str = "instant";
        const ID2: &str = "instant2";
        const ID3: &str = "instant3";

        let name = SubgraphName::new(SUBGRAPH_NAME.to_string()).unwrap();
        let mut subgraph = String::from("none");
        let events = tap_store_events(|| {
            subgraph = store.create_subgraph(name.clone()).unwrap();
        });
        let (current, pending) = subgraph_deployments(store.as_ref(), &subgraph);
        assert!(events.is_empty());
        assert!(current.is_none());
        assert!(pending.is_none());

        // Deploy
        let expected = deploy_event(ID);

        let events = deploy(store.as_ref(), ID, MODE);
        assert_eq!(expected, events);

        let (current, pending) = subgraph_deployments(store.as_ref(), &subgraph);
        assert_eq!(Some(ID), current.as_deref());
        assert!(pending.is_none());

        // Deploying again overwrites current
        let mut expected = deploy_event(ID2);
        expected.insert(removed(MetadataType::SubgraphDeploymentAssignment, ID));

        let events = deploy(store.as_ref(), ID2, MODE);
        assert_eq!(expected, events);

        let (current, pending) = subgraph_deployments(store.as_ref(), &subgraph);
        assert_eq!(Some(ID2), current.as_deref());
        assert!(pending.is_none());

        // Sync deployment
        store
            .deployment_synced(&SubgraphDeploymentId::new(ID2).unwrap())
            .unwrap();

        // Deploying again still overwrites current
        let mut expected = deploy_event(ID3);
        expected.insert(removed(MetadataType::SubgraphDeploymentAssignment, ID2));

        let events = deploy(store.as_ref(), ID3, MODE);
        assert_eq!(expected, events);

        let (current, pending) = subgraph_deployments(store.as_ref(), &subgraph);
        assert_eq!(Some(ID3), current.as_deref());
        assert!(pending.is_none());
    });

    // Test VersionSwitchingMode::Synced
    run_test_sequentially(remove_subgraphs, |store, _| async move {
        const MODE: SubgraphVersionSwitchingMode = SubgraphVersionSwitchingMode::Synced;
        const ID: &str = "synced";
        const ID2: &str = "synced2";
        const ID3: &str = "synced3";

        let name = SubgraphName::new(SUBGRAPH_NAME.to_string()).unwrap();
        let mut subgraph = String::from("none");
        let events = tap_store_events(|| {
            subgraph = store.create_subgraph(name.clone()).unwrap();
        });
        let (current, pending) = subgraph_deployments(store.as_ref(), &subgraph);
        assert!(events.is_empty());
        assert!(current.is_none());
        assert!(pending.is_none());

        // Deploy
        let expected = deploy_event(ID);

        let events = deploy(store.as_ref(), ID, MODE);
        assert_eq!(expected, events);

        let versions = subgraph_versions(store.as_ref(), &subgraph);
        let (current, pending) = subgraph_deployments(store.as_ref(), &subgraph);
        assert_eq!(Some(ID), current.as_deref());
        assert!(pending.is_none());

        // Deploying the same thing again does nothing
        let events = deploy(store.as_ref(), ID, MODE);
        assert!(events.is_empty());
        let versions2 = subgraph_versions(store.as_ref(), &subgraph);
        assert_eq!(versions, versions2);

        // Deploy again, current is not synced, so it gets replaced
        let mut expected = deploy_event(ID2);
        expected.insert(removed(MetadataType::SubgraphDeploymentAssignment, ID));

        let events = deploy(store.as_ref(), ID2, MODE);
        assert_eq!(expected, events);

        let (current, pending) = subgraph_deployments(store.as_ref(), &subgraph);
        assert_eq!(Some(ID2), current.as_deref());
        assert!(pending.is_none());

        // Deploy when current is synced leaves current alone and adds pending
        store
            .deployment_synced(&SubgraphDeploymentId::new(ID2).unwrap())
            .unwrap();
        let expected = deploy_event(ID3);

        let events = deploy(store.as_ref(), ID3, MODE);
        assert_eq!(expected, events);

        let versions = subgraph_versions(store.as_ref(), &subgraph);
        let (current, pending) = subgraph_deployments(store.as_ref(), &subgraph);
        assert_eq!(Some(ID2), current.as_deref());
        assert_eq!(Some(ID3), pending.as_deref());

        // Deploying that same thing again changes nothing
        let events = deploy(store.as_ref(), ID3, MODE);
        assert!(events.is_empty());
        let versions2 = subgraph_versions(store.as_ref(), &subgraph);
        assert_eq!(versions, versions2);

        // Deploy the current version once more; we wind up with current and pending
        // pointing to ID2. That's not ideal, but will be rectified when the
        // next block gets processed and the pending version is promoted to
        // current
        let mut expected = HashSet::new();
        expected.insert(removed(MetadataType::SubgraphDeploymentAssignment, ID3));

        let events = deploy(store.as_ref(), ID2, MODE);
        assert_eq!(expected, events);

        let (current, pending) = subgraph_deployments(store.as_ref(), &subgraph);
        assert_eq!(Some(ID2), current.as_deref());
        assert_eq!(Some(ID2), pending.as_deref());
    })
}
