use std::iter::Peekable;

use super::hasher::Hasher;
use super::merkle_input::MerkleTreeInput;
use super::merkle_multilayer::MerkleMultiLayer;
use super::mixed_degree_decommitment::{DecommitmentNode, MixedDecommitment, PositionInLayer};
use crate::commitment_scheme::merkle_multilayer::MerkleMultiLayerConfig;
use crate::core::fields::{Field, IntoSlice};

/// A mixed degree merkle tree.
///
/// # Example
///
/// ```rust
/// use prover_research::commitment_scheme::merkle_input::MerkleTreeInput;
/// use prover_research::commitment_scheme::mixed_degree_merkle_tree::*;
/// use prover_research::commitment_scheme::blake3_hash::Blake3Hasher;
/// use prover_research::core::fields::m31::M31;
///
/// let mut input = MerkleTreeInput::<M31>::new();
/// let column = vec![M31::from_u32_unchecked(0); 1024];
/// input.insert_column(7, &column);
///
///
/// let mut tree = MixedDegreeMerkleTree::<M31, Blake3Hasher>::new(input,MixedDegreeMerkleTreeConfig {multi_layer_sizes: [5,2].to_vec(),});
/// let root = tree.commit();
pub struct MixedDegreeMerkleTree<'a, F: Field, H: Hasher> {
    input: MerkleTreeInput<'a, F>,
    pub multi_layers: Vec<MerkleMultiLayer<H>>,
}

/// Sets the heights of the multi layers in the tree in ascending order.
pub struct MixedDegreeMerkleTreeConfig {
    pub multi_layer_sizes: Vec<usize>,
}

impl<'a, F: Field, H: Hasher> MixedDegreeMerkleTree<'a, F, H>
where
    F: IntoSlice<H::NativeType>,
{
    pub fn new(input: MerkleTreeInput<'a, F>, config: MixedDegreeMerkleTreeConfig) -> Self {
        let tree_height = input.max_injected_depth();
        Self::validate_config(&config, tree_height);

        let mut layers = Vec::<MerkleMultiLayer<H>>::new();
        let mut current_depth = tree_height;
        for layer_height in config.multi_layer_sizes.into_iter() {
            let layer_config =
                MerkleMultiLayerConfig::new(layer_height, 1 << (current_depth - layer_height));
            layers.push(MerkleMultiLayer::<H>::new(layer_config));
            current_depth -= layer_height;
        }

        MixedDegreeMerkleTree {
            input,
            multi_layers: layers,
        }
    }

    pub fn height(&self) -> usize {
        self.input.max_injected_depth()
    }

    pub fn commit(&mut self) -> H::Hash {
        let tree_height = self.height();
        let mut curr_layer = self.height() - self.multi_layer_height(0);
        // Bottom layer.
        let bottom_multi_layer_input = self.input.split(curr_layer + 1);
        self.multi_layers[0].commit_layer::<F, false>(&bottom_multi_layer_input, &[]);

        // Rest of the tree.
        let mut rebuilt_input = bottom_multi_layer_input;
        for i in 1..self.multi_layers.len() {
            // TODO(Ohad): implement Hash oracle and avoid these copies.
            let prev_hashes = self.multi_layers[i - 1]
                .get_roots()
                .copied()
                .collect::<Vec<H::Hash>>();
            debug_assert_eq!(prev_hashes.len(), 1 << (curr_layer));
            curr_layer -= self.multi_layer_height(i);
            let layer_input = self.input.split(curr_layer + 1);
            self.multi_layers[i].commit_layer::<F, true>(&layer_input, &prev_hashes);
            rebuilt_input.prepend(layer_input);
        }

        let mut top_layer_roots = self.multi_layers.last().unwrap().get_roots();
        let root = top_layer_roots
            .next()
            .expect("Top layer should have exactly one root")
            .to_owned();
        debug_assert_eq!(top_layer_roots.count(), 0);
        debug_assert_eq!(rebuilt_input.max_injected_depth(), tree_height);
        self.input = rebuilt_input;
        root
    }

    pub fn decommit(&self, mut queries: Vec<usize>) -> MixedDecommitment<F, H> {
        let mut decommitment_layers = Vec::<Vec<DecommitmentNode<F, H>>>::with_capacity(self.height());
        
        // Leaf layer.
        let leaf_layer_decommitment = self._decommit_leaf_layer(queries.iter().copied().peekable());
        decommitment_layers.push(leaf_layer_decommitment);

        // Rest of the tree.
        for i in (1..self.height()).rev() {
            let layer_decommitment = self._decommit_intermediate_layer(
                i,
                queries.iter().copied().peekable(),
            );
            decommitment_layers.push(layer_decommitment);
            queries = Self::get_parent_indices(queries);
        }

        MixedDecommitment {
            decommitment_layers,
        }
    }

    fn get_parent_indices(children: Vec<usize>) -> Vec<usize> {
        let mut parent_indices = children.into_iter().map(|c| c / 2).collect::<Vec<_>>();
        parent_indices.dedup();
        parent_indices
    }
    

    pub fn get_hash_at(&self, layer_depth: usize, position: usize) -> H::Hash {
        // Determine correct multilayer
        let mut depth_accumulator = layer_depth;
        for multi_layer in self.multi_layers.iter().rev() {
            let multi_layer_height = multi_layer.config.sub_tree_height;
            if multi_layer_height > depth_accumulator {
                return multi_layer.get_hash_value(depth_accumulator, position);
            }
            depth_accumulator -= multi_layer_height;
        }
        panic!()
    }

    // TODO(Ohad): use in decommit and remove '_'.
    fn _decommit_intermediate_layer(
        &self,
        layer_depth: usize,
        mut current_queried_indices: Peekable<impl Iterator<Item = usize>>,
    ) -> Vec<DecommitmentNode<F, H>> {
        let mut proof_layer = Vec::<DecommitmentNode<F, H>>::new();
        while let Some(q) = current_queried_indices.next() {
            let sibling_index = q ^ 1;
            let hash_witness = match current_queried_indices.peek() {
                // If both children are in the layer, only injected elements are needed
                // to calculate the parent.
                Some(next_q) if *next_q == sibling_index => {
                    current_queried_indices.next();
                    None
                }
                _ => Some(self.get_hash_at(layer_depth, sibling_index)),
            };
            let bag_index = q / 2;
            let injected_elements = self.input.get_injected_elements(layer_depth, bag_index);
            if hash_witness.is_some() || !injected_elements.is_empty() {
                let position_in_layer = PositionInLayer::new_child(sibling_index);
                proof_layer.push(DecommitmentNode {
                    position_in_layer,
                    hash: hash_witness,
                    injected_elements,
                });
            }
        }
        proof_layer
    }

    fn _decommit_leaf_layer(
        &self,
        leaf_layer_indices: Peekable<impl Iterator<Item = usize>>,
    ) -> Vec<DecommitmentNode<F, H>> {
        let mut leaf_layer = Vec::<DecommitmentNode<F, H>>::new();
        for q in leaf_layer_indices {
            let position_in_layer = PositionInLayer::Leaf(q);
            let injected_elements = self.input.get_injected_elements(self.height(), q);
            leaf_layer.push(DecommitmentNode {
                hash: None,
                position_in_layer,
                injected_elements,
            });
        }
        leaf_layer
    }

    pub fn root(&self) -> H::Hash {
        match &self.multi_layers.last() {
            Some(top_layer) => {
                let mut roots = top_layer.get_roots();
                assert_eq!(roots.len(), 1, "Top layer should have exactly one root");
                *roots.next().unwrap()
            }
            None => panic!("Empty tree!"),
        }
    }

    fn validate_config(config: &MixedDegreeMerkleTreeConfig, tree_height: usize) {
        let config_tree_height = config.multi_layer_sizes.iter().sum::<usize>();
        assert_eq!(
            config.multi_layer_sizes.iter().sum::<usize>(),
            tree_height,
            "Sum of the layer heights {} does not match merkle input size {}.",
            config_tree_height,
            tree_height
        );
    }

    fn multi_layer_height(&self, layer_index: usize) -> usize {
        assert!(layer_index < self.multi_layers.len());
        self.multi_layers[layer_index].config.sub_tree_height
    }
}

#[cfg(test)]
mod tests {
    use super::{MixedDegreeMerkleTree, MixedDegreeMerkleTreeConfig};
    use crate::commitment_scheme::blake2_hash::Blake2sHasher;
    use crate::commitment_scheme::blake3_hash::Blake3Hasher;
    use crate::commitment_scheme::hasher::Hasher;
    use crate::core::fields::m31::M31;
    use crate::m31;

    #[test]
    fn new_mixed_degree_merkle_tree_test() {
        let mut input = super::MerkleTreeInput::<M31>::new();
        let column = vec![M31::from_u32_unchecked(0); 1 << 12];
        input.insert_column(12, &column);

        let multi_layer_sizes = [5, 4, 3].to_vec();
        let tree = MixedDegreeMerkleTree::<M31, Blake2sHasher>::new(
            input,
            MixedDegreeMerkleTreeConfig {
                multi_layer_sizes: multi_layer_sizes.clone(),
            },
        );

        let mut remaining_height = multi_layer_sizes.iter().sum::<usize>();
        multi_layer_sizes
            .iter()
            .enumerate()
            .for_each(|(i, layer_height)| {
                assert_eq!(tree.multi_layers[i].config.sub_tree_height, *layer_height);
                assert_eq!(
                    tree.multi_layers[i].config.n_sub_trees,
                    1 << (remaining_height - layer_height)
                );
                remaining_height -= layer_height;
            });
    }

    #[test]
    #[should_panic]
    fn new_mixed_degree_merkle_tree_bad_config_test() {
        let mut input = super::MerkleTreeInput::<M31>::new();
        let column = vec![M31::from_u32_unchecked(0); 4096];
        input.insert_column(12, &column);

        // This should panic because the sum of the layer heights is not equal to the tree height
        // deferred by the input.
        MixedDegreeMerkleTree::<M31, Blake2sHasher>::new(
            input,
            MixedDegreeMerkleTreeConfig {
                multi_layer_sizes: [5, 4, 2].to_vec(),
            },
        );
    }

    fn hash_symmetric_path<H: Hasher>(
        initial_value: &[H::NativeType],
        path_length: usize,
    ) -> H::Hash {
        (1..path_length).fold(H::hash(initial_value), |curr_hash, _| {
            H::concat_and_hash(&curr_hash, &curr_hash)
        })
    }

    #[test]
    fn commit_test() {
        const TREE_HEIGHT: usize = 8;
        const INJECT_DEPTH: usize = 3;
        let mut input = super::MerkleTreeInput::<M31>::new();
        let base_column = vec![M31::from_u32_unchecked(0); 1 << (TREE_HEIGHT)];
        let injected_column = vec![M31::from_u32_unchecked(1); 1 << (INJECT_DEPTH - 1)];
        input.insert_column(TREE_HEIGHT + 1, &base_column);
        input.insert_column(INJECT_DEPTH, &injected_column);
        let mut tree = MixedDegreeMerkleTree::<M31, Blake3Hasher>::new(
            input,
            MixedDegreeMerkleTreeConfig {
                multi_layer_sizes: [5, 2, 2].to_vec(),
            },
        );
        let expected_hash_at_injected_depth = hash_symmetric_path::<Blake3Hasher>(
            0_u32.to_le_bytes().as_ref(),
            TREE_HEIGHT + 1 - INJECT_DEPTH,
        );
        let mut sack_at_injected_depth = expected_hash_at_injected_depth.as_ref().to_vec();
        sack_at_injected_depth.extend(expected_hash_at_injected_depth.as_ref().to_vec());
        sack_at_injected_depth.extend(1u32.to_le_bytes());
        let expected_result =
            hash_symmetric_path::<Blake3Hasher>(sack_at_injected_depth.as_ref(), INJECT_DEPTH);

        let root = tree.commit();
        assert_eq!(root, expected_result);
    }

    #[test]
    fn get_hash_at_test() {
        const TREE_HEIGHT: usize = 3;
        let mut input = super::MerkleTreeInput::<M31>::new();
        let base_column = (0..4).map(M31::from_u32_unchecked).collect::<Vec<M31>>();
        input.insert_column(TREE_HEIGHT, &base_column);
        let mut tree = MixedDegreeMerkleTree::<M31, Blake3Hasher>::new(
            input,
            MixedDegreeMerkleTreeConfig {
                multi_layer_sizes: [2, 1].to_vec(),
            },
        );
        let root = tree.commit();
        assert_eq!(root, tree.get_hash_at(0, 0));

        let mut hasher = Blake3Hasher::new();
        hasher.update(&0_u32.to_le_bytes());
        // hasher.update(&1_u32.to_le_bytes());
        let expected_hash_at_2_0 = hasher.finalize_reset();
        let hash_at_2_0 = tree.get_hash_at(2, 0);
        assert_eq!(hash_at_2_0, expected_hash_at_2_0);

        hasher.update(&2_u32.to_le_bytes());
        let expected_hash_at_2_2 = hasher.finalize_reset();
        let hash_at_2_2 = tree.get_hash_at(2, 2);
        assert_eq!(hash_at_2_2, expected_hash_at_2_2);
        hasher.update(&3_u32.to_le_bytes());
        let expected_hash_at_2_3 = hasher.finalize_reset();
        let hash_at_2_3 = tree.get_hash_at(2, 3);
        assert_eq!(hash_at_2_3, expected_hash_at_2_3);

        let expected_parent_of_2_2_and_2_3 =
            Blake3Hasher::concat_and_hash(&expected_hash_at_2_2, &expected_hash_at_2_3);
        let parent_of_2_2_and_2_3 = tree.get_hash_at(1, 1);
        assert_eq!(parent_of_2_2_and_2_3, expected_parent_of_2_2_and_2_3);
    }

    #[test]
    #[should_panic]
    fn get_hash_at_invalid_layer_test() {
        const TREE_HEIGHT: usize = 3;
        let mut input = super::MerkleTreeInput::<M31>::new();
        let base_column = (0..4).map(M31::from_u32_unchecked).collect::<Vec<M31>>();
        input.insert_column(TREE_HEIGHT, &base_column);
        let tree = MixedDegreeMerkleTree::<M31, Blake3Hasher>::new(
            input,
            MixedDegreeMerkleTreeConfig {
                multi_layer_sizes: [2, 1].to_vec(),
            },
        );
        tree.get_hash_at(4, 0);
    }

    #[test]
    fn decommit_intermediate_layer_test() {
        const TREE_HEIGHT: usize = 3;
        let mut input = super::MerkleTreeInput::<M31>::new();
        let base_column = (0..4).map(M31::from_u32_unchecked).collect::<Vec<M31>>();
        let inject_at_depth_1_column = vec![m31!(1)];
        input.insert_column(TREE_HEIGHT, &base_column);
        input.insert_column(1, &inject_at_depth_1_column);
        let mut tree = MixedDegreeMerkleTree::<M31, Blake3Hasher>::new(
            input,
            MixedDegreeMerkleTreeConfig {
                multi_layer_sizes: [2, 1].to_vec(),
            },
        );
        tree.commit();

        let queried_indices = vec![0, 2, 3].into_iter().peekable();
        let parent_indices = vec![0, 1].into_iter().peekable();
        let proof_layer_depth_2 =
            tree._decommit_intermediate_layer(TREE_HEIGHT - 1, queried_indices);
        let proof_layer_depth_1 =
            tree._decommit_intermediate_layer(TREE_HEIGHT - 2, parent_indices);

        // Only 1 bag in that layer.
        assert_eq!(proof_layer_depth_1.len(), 1);

        // Indices are siblings hence no hash in that node.
        assert_eq!(proof_layer_depth_1[0].hash, None);
        assert_eq!(proof_layer_depth_1[0].injected_elements, vec![m31!(1)]);

        // 2,3 are siblings, and no injected elements in that layer so bag is not included.
        assert_eq!(proof_layer_depth_1.len(), 1);
        assert_eq!(proof_layer_depth_1[0].hash, None);

        let mut hasher = Blake3Hasher::new();
        hasher.update(&1_u32.to_le_bytes());
        let expected_hash_at_2_0 = hasher.finalize_reset();
        assert_eq!(proof_layer_depth_2[0].hash, Some(expected_hash_at_2_0));
        assert_eq!(proof_layer_depth_2[0].injected_elements, vec![]);
    }

    #[test]
    fn decommit_test() {
        const TREE_HEIGHT: usize = 4;
        let mut input = super::MerkleTreeInput::<M31>::new();
        let base_column = (0..8).map(M31::from_u32_unchecked).collect::<Vec<M31>>();
        input.insert_column(TREE_HEIGHT, &base_column);
        let mut tree = MixedDegreeMerkleTree::<M31, Blake3Hasher>::new(
            input,
            MixedDegreeMerkleTreeConfig {
                multi_layer_sizes: [1, 2, 1].to_vec(),
            },
        );
        tree.commit();
        let leaf_layer_queries = vec![0,2,7];
        let decommitment = tree.decommit(leaf_layer_queries);
        
        let leaf_layer_decommitment = &decommitment.decommitment_layers[0];
        assert_eq!(leaf_layer_decommitment.len(), 3);
        assert_eq!(leaf_layer_decommitment[0].injected_elements, vec![m31!(0)]);
        assert_eq!(leaf_layer_decommitment[1].injected_elements, vec![m31!(2)]);
        assert_eq!(leaf_layer_decommitment[2].injected_elements, vec![m31!(7)]);

        let expected_hash_at_1_1 = Blake3Hasher::hash(&1_u32.to_le_bytes());
        let expected_hash_at_1_3 = Blake3Hasher::hash(&3_u32.to_le_bytes());
        let expected_hash_at_1_6 = Blake3Hasher::hash(&6_u32.to_le_bytes());
        let decommitment_layer_1 = &decommitment.decommitment_layers[1];
        assert_eq!(decommitment_layer_1.len(), 3);
        assert_eq!(decommitment_layer_1[0].hash, Some(expected_hash_at_1_1));
        assert_eq!(decommitment_layer_1[1].hash, Some(expected_hash_at_1_3));
        assert_eq!(decommitment_layer_1[2].hash, Some(expected_hash_at_1_6));

        let expected_hash_at_2_1 = Blake3Hasher::concat_and_hash(
            &Blake3Hasher::hash(&2_u32.to_le_bytes()),
            &Blake3Hasher::hash(&3_u32.to_le_bytes()),
        );
        let expected_hash_at_2_2 = Blake3Hasher::concat_and_hash(
            &Blake3Hasher::hash(&4_u32.to_le_bytes()),
            &Blake3Hasher::hash(&5_u32.to_le_bytes()),
        );
        let expected_hash_at_2_3 = Blake3Hasher::concat_and_hash(
            &Blake3Hasher::hash(&6_u32.to_le_bytes()),
            &Blake3Hasher::hash(&7_u32.to_le_bytes()),
        );
        let expected_hash_at_3_1 = Blake3Hasher::concat_and_hash(
            &expected_hash_at_2_2,
            &expected_hash_at_2_3,
        );

        println!("{}", decommitment);
        let layer_1_decommitment = &decommitment.decommitment_layers[1];
        assert_eq!(layer_1_decommitment.len(), 3);


        let layer_2_decommitment = &decommitment.decommitment_layers[2];
        assert_eq!(layer_2_decommitment.len(), 1);
        assert_eq!(layer_2_decommitment[0].hash, Some(expected_hash_at_2_1));
        let layer_3_decommitment = &decommitment.decommitment_layers[3];
        assert_eq!(layer_3_decommitment.len(), 1);
        assert_eq!(layer_3_decommitment[0].hash, Some(expected_hash_at_3_1));
    }
}
