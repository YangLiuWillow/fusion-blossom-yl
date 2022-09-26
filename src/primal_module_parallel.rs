//! Parallel Primal Module
//! 
//! A parallel implementation of the primal module, by calling functions provided by the serial primal module
//!

use super::util::*;
use serde::{Serialize, Deserialize};
use crate::rayon::prelude::*;
use super::primal_module::*;
use super::primal_module_serial::*;
use super::dual_module_parallel::*;
use super::visualize::*;
use super::dual_module::*;
use std::sync::Arc;
use std::ops::DerefMut;
use std::time::Instant;


pub struct PrimalModuleParallel {
    /// the basic wrapped serial modules at the beginning, afterwards the fused units are appended after them
    pub units: Vec<PrimalModuleParallelUnitPtr>,
    /// local configuration
    pub config: PrimalModuleParallelConfig,
    /// partition information generated by the config
    pub partition_info: Arc<PartitionInfo>,
    /// thread pool used to execute async functions in parallel
    pub thread_pool: Arc<rayon::ThreadPool>,
    /// the time of calling [`PrimalModuleParallel::parallel_solve_step_callback`] method
    pub last_solve_start_time: Instant,
}

pub struct PrimalModuleParallelUnit {
    /// the index
    pub unit_index: usize,
    /// the dual module interface, for constant-time clear
    pub interface_ptr: DualModuleInterfacePtr,
    /// partition information generated by the config
    pub partition_info: Arc<PartitionInfo>,
    /// whether it's active or not; some units are "placeholder" units that are not active until they actually fuse their children
    pub is_active: bool,
    /// the owned serial primal module
    pub serial_module: PrimalModuleSerialPtr,
    /// left and right children dual modules
    pub children: Option<(PrimalModuleParallelUnitWeak, PrimalModuleParallelUnitWeak)>,
    /// parent dual module
    pub parent: Option<PrimalModuleParallelUnitWeak>,
    /// record the time of events
    pub event_time: Option<PrimalModuleParallelUnitEventTime>,
}

pub type PrimalModuleParallelUnitPtr = ArcRwLock<PrimalModuleParallelUnit>;
pub type PrimalModuleParallelUnitWeak = WeakRwLock<PrimalModuleParallelUnit>;

impl std::fmt::Debug for PrimalModuleParallelUnitPtr {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let unit = self.read_recursive();
        write!(f, "{}", unit.unit_index)
    }
}

impl std::fmt::Debug for PrimalModuleParallelUnitWeak {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        self.upgrade_force().fmt(f)
    }
}

/// the time of critical events, for profiling purposes
#[derive(Debug, Clone, Serialize)]
pub struct PrimalModuleParallelUnitEventTime {
    /// unit starts executing
    pub start: f64,
    /// unit done children execution
    pub children_return: f64,
    /// unit ends executing
    pub end: f64,
}

impl Default for PrimalModuleParallelUnitEventTime {
    fn default() -> Self {
        Self::new()
    }
}

impl PrimalModuleParallelUnitEventTime {
    pub fn new() -> Self {
        Self {
            start: 0.,
            children_return: 0.,
            end: 0.,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrimalModuleParallelConfig {
    /// enable async execution of dual operations; only used when calling top-level operations, not used in individual units
    #[serde(default = "primal_module_parallel_default_configs::thread_pool_size")]
    pub thread_pool_size: usize,
    /// debug by sequentially run the fusion tasks, user must enable this for visualizer to work properly during the execution
    #[serde(default = "primal_module_parallel_default_configs::debug_sequential")]
    pub debug_sequential: bool,
}

impl Default for PrimalModuleParallelConfig {
    fn default() -> Self { serde_json::from_value(json!({})).unwrap() }
}

pub mod primal_module_parallel_default_configs {
    pub fn thread_pool_size() -> usize { 0 }  // by default to the number of CPU cores
    // pub fn thread_pool_size() -> usize { 1 }  // debug: use a single core
    pub fn debug_sequential() -> bool { false }  // by default enabled: only disable when you need to debug and get visualizer to work
}

impl PrimalModuleParallel {

    /// recommended way to create a new instance, given a customized configuration
    pub fn new_config(initializer: &SolverInitializer, partition_info: Arc<PartitionInfo>, config: PrimalModuleParallelConfig) -> Self {
        let mut thread_pool_builder = rayon::ThreadPoolBuilder::new();
        if config.thread_pool_size != 0 {
            thread_pool_builder = thread_pool_builder.num_threads(config.thread_pool_size);
        }
        let thread_pool = thread_pool_builder.build().expect("creating thread pool failed");
        let mut units = vec![];
        let unit_count = partition_info.units.len();
        thread_pool.scope(|_| {
            (0..unit_count).into_par_iter().map(|unit_index| {
                // println!("unit_index: {unit_index}");
                let primal_module = PrimalModuleSerialPtr::new_empty(initializer);
                PrimalModuleParallelUnitPtr::new_wrapper(primal_module, unit_index, Arc::clone(&partition_info))
            }).collect_into_vec(&mut units);
        });
        // fill in the children and parent references
        for unit_index in 0..unit_count {
            let mut unit = units[unit_index].write();
            if let Some((left_children_index, right_children_index)) = &partition_info.units[unit_index].children {
                unit.children = Some((units[*left_children_index].downgrade(), units[*right_children_index].downgrade()))
            }
            if let Some(parent_index) = &partition_info.units[unit_index].parent {
                unit.parent = Some(units[*parent_index].downgrade());
            }
        }
        Self {
            units,
            config,
            partition_info,
            thread_pool: Arc::new(thread_pool),
            last_solve_start_time: Instant::now(),
        }
    }

}

impl PrimalModuleImpl for PrimalModuleParallel {

    fn new_empty(initializer: &SolverInitializer) -> Self {
        Self::new_config(initializer, PartitionConfig::default(initializer.vertex_num).into_info(), PrimalModuleParallelConfig::default())
    }

    fn clear(&mut self) {
        self.thread_pool.scope(|_| {
            self.units.par_iter().enumerate().for_each(|(unit_idx, unit_ptr)| {
                let mut unit = unit_ptr.write();
                let partition_unit_info = &unit.partition_info.units[unit_idx];
                let is_active = partition_unit_info.children.is_none();
                unit.clear();
                unit.is_active = is_active;
            });
        });
    }

    fn load_syndrome_dual_node(&mut self, _dual_node_ptr: &DualNodePtr) {
        panic!("load interface directly into the parallel primal module is forbidden, use `parallel_solve` instead");
    }

    fn resolve<D: DualModuleImpl>(&mut self, _group_max_update_length: GroupMaxUpdateLength, _interface: &DualModuleInterfacePtr, _dual_module: &mut D) {
        panic!("parallel primal module cannot handle global resolve requests, use `parallel_solve` instead");
    }

    fn intermediate_matching<D: DualModuleImpl>(&mut self, interface: &DualModuleInterfacePtr, dual_module: &mut D) -> IntermediateMatching {
        let mut intermediate_matching = IntermediateMatching::new();
        for unit_ptr in self.units.iter() {
            let mut unit = unit_ptr.write();
            if !unit.is_active { continue }  // do not visualize inactive units
            intermediate_matching.append(&mut unit.serial_module.intermediate_matching(interface, dual_module));
        }
        intermediate_matching
    }

    fn generate_profiler_report(&self) -> serde_json::Value {
        let event_time_vec: Vec<_> = self.units.iter().map(|ptr| ptr.read_recursive().event_time.clone()).collect();
        json!({
            "event_time_vec": event_time_vec,
        })
    }

}

impl PrimalModuleParallel {

    pub fn parallel_solve<DualSerialModule: DualModuleImpl + Send + Sync>
            (&mut self, syndrome_pattern: &SyndromePattern, parallel_dual_module: &mut DualModuleParallel<DualSerialModule>) {
        self.parallel_solve_step_callback(syndrome_pattern, parallel_dual_module, |_, _, _, _| {})
    }

    pub fn parallel_solve_visualizer<DualSerialModule: DualModuleImpl + Send + Sync + FusionVisualizer>
            (&mut self, syndrome_pattern: &SyndromePattern, parallel_dual_module: &mut DualModuleParallel<DualSerialModule>
            , visualizer: Option<&mut Visualizer>) {
        if let Some(visualizer) = visualizer {
            self.parallel_solve_step_callback(syndrome_pattern, parallel_dual_module
                , |interface_ptr, dual_module, primal_module, group_max_update_length| {
                    if let Some(group_max_update_length) = group_max_update_length {
                        println!("group_max_update_length: {:?}", group_max_update_length);
                        if let Some(length) = group_max_update_length.get_none_zero_growth() {
                            visualizer.snapshot_combined(format!("grow {length}"), vec![interface_ptr, dual_module, primal_module]).unwrap();
                        } else {
                            let first_conflict = format!("{:?}", group_max_update_length.peek().unwrap());
                            visualizer.snapshot_combined(format!("resolve {first_conflict}"), vec![interface_ptr, dual_module, primal_module]).unwrap();
                        };
                    } else {
                        visualizer.snapshot_combined("unit solved".to_string(), vec![interface_ptr, dual_module, primal_module]).unwrap();
                    }
                });
            let last_unit = self.units.last().unwrap().read_recursive();
            visualizer.snapshot_combined("solved".to_string(), vec![&last_unit.interface_ptr, parallel_dual_module, self]).unwrap();
        } else {
            self.parallel_solve(syndrome_pattern, parallel_dual_module);
        }
    }

    pub fn parallel_solve_step_callback<DualSerialModule: DualModuleImpl + Send + Sync, F: Send + Sync>
            (&mut self, syndrome_pattern: &SyndromePattern, parallel_dual_module: &mut DualModuleParallel<DualSerialModule>, mut callback: F)
            where F: FnMut(&DualModuleInterfacePtr, &DualModuleParallelUnit<DualSerialModule>, &PrimalModuleSerialPtr, Option<&GroupMaxUpdateLength>) {
        let last_unit_ptr = self.units.last().unwrap().clone();
        let thread_pool = Arc::clone(&self.thread_pool);
        self.last_solve_start_time = Instant::now();
        thread_pool.scope(|_| {
            last_unit_ptr.iterative_solve_step_callback(self, PartitionedSyndromePattern::new(syndrome_pattern), parallel_dual_module, &mut Some(&mut callback))
        })
    }

}

impl FusionVisualizer for PrimalModuleParallel {
    fn snapshot(&self, abbrev: bool) -> serde_json::Value {
        // do the sanity check first before taking snapshot
        // self.sanity_check().unwrap();
        let mut value = json!({});
        for unit_ptr in self.units.iter() {
            let unit = unit_ptr.read_recursive();
            if !unit.is_active { continue }  // do not visualize inactive units
            let value_2 = unit.snapshot(abbrev);
            snapshot_combine_values(&mut value, value_2, abbrev);
        }
        value
    }
}

impl FusionVisualizer for PrimalModuleParallelUnit {
    fn snapshot(&self, abbrev: bool) -> serde_json::Value {
        self.serial_module.snapshot(abbrev)
    }
}

impl PrimalModuleParallelUnitPtr {

    /// create a simple wrapper over a serial dual module
    pub fn new_wrapper(serial_module: PrimalModuleSerialPtr, unit_index: usize, partition_info: Arc<PartitionInfo>) -> Self {
        let partition_unit_info = &partition_info.units[unit_index];
        let is_active = partition_unit_info.children.is_none();
        Self::new_value(PrimalModuleParallelUnit {
            unit_index,
            interface_ptr: DualModuleInterfacePtr::new_empty(),
            partition_info,
            is_active,  // only activate the leaves in the dependency tree
            serial_module,
            children: None,  // to be filled later
            parent: None,  // to be filled later
            event_time: None,
        })
    }

    /// call on the last primal node, and it will spawn tasks on the previous ones
    fn iterative_solve_step_callback<DualSerialModule: DualModuleImpl + Send + Sync, F: Send + Sync>(&self, primal_module_parallel: &PrimalModuleParallel
                , partitioned_syndrome_pattern: PartitionedSyndromePattern, parallel_dual_module: &DualModuleParallel<DualSerialModule>, callback: &mut Option<&mut F>)
            where F: FnMut(&DualModuleInterfacePtr, &DualModuleParallelUnit<DualSerialModule>, &PrimalModuleSerialPtr, Option<&GroupMaxUpdateLength>) {
        let mut primal_unit = self.write();
        let mut event_time = PrimalModuleParallelUnitEventTime::new();
        event_time.start = primal_module_parallel.last_solve_start_time.elapsed().as_secs_f64();
        let dual_module_ptr = parallel_dual_module.get_unit(primal_unit.unit_index);
        let mut dual_unit = dual_module_ptr.write();
        // only when sequentially running the tasks will the callback take effect, otherwise it's unsafe to execute it from multiple threads
        let debug_sequential = primal_module_parallel.config.debug_sequential;
        if let Some((left_child_weak, right_child_weak)) = primal_unit.children.as_ref() {
            assert!(!primal_unit.is_active, "parent must be inactive at the time of solving children");
            let partition_unit_info = &primal_unit.partition_info.units[primal_unit.unit_index];
            let (syndrome_range, (left_partitioned, right_partitioned)) = partitioned_syndrome_pattern.partition(partition_unit_info);
            if debug_sequential {
                left_child_weak.upgrade_force().iterative_solve_step_callback(primal_module_parallel, left_partitioned
                    , parallel_dual_module, callback);
                right_child_weak.upgrade_force().iterative_solve_step_callback(primal_module_parallel, right_partitioned
                    , parallel_dual_module, callback);
            } else {
                rayon::join(|| {
                    left_child_weak.upgrade_force().iterative_solve_step_callback::<DualSerialModule, F>(primal_module_parallel, left_partitioned
                        , parallel_dual_module, &mut None)
                }, || {
                    right_child_weak.upgrade_force().iterative_solve_step_callback::<DualSerialModule, F>(primal_module_parallel, right_partitioned
                        , parallel_dual_module, &mut None)
                });
            };
            event_time.children_return = primal_module_parallel.last_solve_start_time.elapsed().as_secs_f64();
            {  // set children to inactive to avoid being solved twice
                for child_weak in [left_child_weak, right_child_weak] {
                    let child_ptr = child_weak.upgrade_force();
                    let mut child = child_ptr.write();
                    assert!(child.is_active, "cannot fuse inactive children");
                    child.is_active = false;
                }
            }
            primal_unit.fuse(&mut dual_unit);
            if let Some(callback) = callback.as_mut() {  // do callback before actually breaking the matched pairs, for ease of visualization
                callback(&primal_unit.interface_ptr, &dual_unit, &primal_unit.serial_module, None);
            }
            primal_unit.break_matching_with_mirror(dual_unit.deref_mut());
            let interface_ptr = primal_unit.interface_ptr.clone();
            for syndrome_index in syndrome_range.iter() {
                let syndrome_vertex = partitioned_syndrome_pattern.syndrome_pattern.syndrome_vertices[syndrome_index];
                primal_unit.serial_module.load_syndrome(syndrome_vertex, &interface_ptr, dual_unit.deref_mut());
            }
            primal_unit.serial_module.solve_step_callback_interface_loaded(&interface_ptr, dual_unit.deref_mut()
                , |interface, dual_module, primal_module, group_max_update_length| {
                    if let Some(callback) = callback.as_mut() {
                        callback(interface, dual_module, primal_module, Some(group_max_update_length));
                    }
                });
        } else {  // this is a leaf, proceed it as normal serial one
            event_time.children_return = primal_module_parallel.last_solve_start_time.elapsed().as_secs_f64();  // no children
            assert!(primal_unit.is_active, "leaf must be active to be solved");
            let syndrome_pattern = partitioned_syndrome_pattern.expand();
            let interface_ptr = primal_unit.interface_ptr.clone();
            primal_unit.serial_module.solve_step_callback(&interface_ptr, &syndrome_pattern, dual_unit.deref_mut()
                , |interface, dual_module, primal_module, group_max_update_length| {
                    if let Some(callback) = callback.as_mut() {
                        callback(interface, dual_module, primal_module, Some(group_max_update_length));
                    }
                });
        };
        if let Some(callback) = callback.as_mut() {
            callback(&primal_unit.interface_ptr, &dual_unit, &primal_unit.serial_module, None);
        }
        primal_unit.is_active = true;
        event_time.end = primal_module_parallel.last_solve_start_time.elapsed().as_secs_f64();
        primal_unit.event_time = Some(event_time);
    }

}

impl PrimalModuleParallelUnit {

    /// fuse two units together, by copying the right child's content into the left child's content and resolve index;
    /// note that this operation doesn't update on the dual module, call [`Self::break_matching_with_mirror`] if needed
    pub fn fuse<DualSerialModule: DualModuleImpl + Send + Sync>(&mut self, dual_unit: &mut DualModuleParallelUnit<DualSerialModule>) {
        let (left_child_ptr, right_child_ptr) = (self.children.as_ref().unwrap().0.upgrade_force(), self.children.as_ref().unwrap().1.upgrade_force());
        let left_child = left_child_ptr.read_recursive();
        let right_child = right_child_ptr.read_recursive();
        dual_unit.fuse(&self.interface_ptr, (&left_child.interface_ptr, &right_child.interface_ptr));
        self.serial_module.fuse(&left_child.serial_module, &right_child.serial_module);
    }

    /// break the matched pairs of interface vertices
    pub fn break_matching_with_mirror(&mut self, dual_module: &mut impl DualModuleImpl) {
        // use `possible_break` to efficiently break those
        let mut possible_break = vec![];
        let module = self.serial_module.read_recursive();
        for node_index in module.possible_break.iter() {
            let primal_node_ptr = module.get_node(*node_index);
            if let Some(primal_node_ptr) = primal_node_ptr {
                let mut primal_node = primal_node_ptr.write();
                if let Some((MatchTarget::VirtualVertex(vertex_index), _)) = &primal_node.temporary_match {
                    if self.partition_info.vertex_to_owning_unit[*vertex_index] == self.unit_index {
                        primal_node.temporary_match = None;
                        self.interface_ptr.set_grow_state(&primal_node.origin.upgrade_force(), DualNodeGrowState::Grow, dual_module);
                    } else {  // still possible break
                        possible_break.push(*node_index);
                    }
                }
            }
        }
        drop(module);
        self.serial_module.write().possible_break = possible_break;
    }

}

impl PrimalModuleImpl for PrimalModuleParallelUnit {

    fn new_empty(_initializer: &SolverInitializer) -> Self {
        panic!("creating parallel unit directly from initializer is forbidden, use `PrimalModuleParallel::new` instead");
    }

    fn clear(&mut self) {
        self.serial_module.clear();
        self.interface_ptr.clear();
    }

    fn load(&mut self, interface_ptr: &DualModuleInterfacePtr) {
        self.serial_module.load(interface_ptr)
    }

    fn load_syndrome_dual_node(&mut self, dual_node_ptr: &DualNodePtr) {
        self.serial_module.load_syndrome_dual_node(dual_node_ptr)
    }

    fn resolve<D: DualModuleImpl>(&mut self, group_max_update_length: GroupMaxUpdateLength, interface: &DualModuleInterfacePtr, dual_module: &mut D) {
        self.serial_module.resolve(group_max_update_length, interface, dual_module)
    }

    fn intermediate_matching<D: DualModuleImpl>(&mut self, interface: &DualModuleInterfacePtr, dual_module: &mut D) -> IntermediateMatching {
        self.serial_module.intermediate_matching(interface, dual_module)
    }

}

#[cfg(test)]
pub mod tests {
    use super::*;
    use super::super::example::*;
    use super::super::dual_module_serial::*;
    use std::sync::Arc;

    pub fn primal_module_parallel_basic_standard_syndrome_optional_viz<F>(mut code: impl ExampleCode, visualize_filename: Option<String>
            , mut syndrome_vertices: Vec<VertexIndex>, final_dual: Weight, partition_func: F, reordered_vertices: Option<Vec<VertexIndex>>)
            -> (PrimalModuleParallel, DualModuleParallel<DualModuleSerial>) where F: Fn(&SolverInitializer, &mut PartitionConfig) {
        println!("{syndrome_vertices:?}");
        if let Some(reordered_vertices) = &reordered_vertices {
            code.reorder_vertices(reordered_vertices);
            syndrome_vertices = translated_syndrome_to_reordered(reordered_vertices, &syndrome_vertices);
        }
        let mut visualizer = match visualize_filename.as_ref() {
            Some(visualize_filename) => {
                let mut visualizer = Visualizer::new(Some(visualize_data_folder() + visualize_filename.as_str())).unwrap();
                visualizer.set_positions(code.get_positions(), true);  // automatic center all nodes
                print_visualize_link(&visualize_filename);
                Some(visualizer)
            }, None => None
        };
        let initializer = code.get_initializer();
        let mut partition_config = PartitionConfig::default(initializer.vertex_num);
        partition_func(&initializer, &mut partition_config);
        let partition_info = partition_config.into_info();
        let mut dual_module = DualModuleParallel::new_config(&initializer, Arc::clone(&partition_info), DualModuleParallelConfig::default());
        let mut primal_config = PrimalModuleParallelConfig::default();
        primal_config.debug_sequential = true;
        let mut primal_module = PrimalModuleParallel::new_config(&initializer, Arc::clone(&partition_info), primal_config);
        code.set_syndrome_vertices(&syndrome_vertices);
        primal_module.parallel_solve_visualizer(&code.get_syndrome(), &mut dual_module, visualizer.as_mut());
        assert_eq!(primal_module.units.last().unwrap().read_recursive().interface_ptr.sum_dual_variables(), final_dual * 2, "unexpected final dual variable sum");
        (primal_module, dual_module)
    }

    pub fn primal_module_parallel_standard_syndrome<F>(code: impl ExampleCode, visualize_filename: String, syndrome_vertices: Vec<VertexIndex>
            , final_dual: Weight, partition_func: F, reordered_vertices: Option<Vec<VertexIndex>>)
            -> (PrimalModuleParallel, DualModuleParallel<DualModuleSerial>) where F: Fn(&SolverInitializer, &mut PartitionConfig) {
        primal_module_parallel_basic_standard_syndrome_optional_viz(code, Some(visualize_filename), syndrome_vertices, final_dual, partition_func, reordered_vertices)
    }

    /// test a simple case
    #[test]
    fn primal_module_parallel_basic_1() {  // cargo test primal_module_parallel_basic_1 -- --nocapture
        let visualize_filename = format!("primal_module_parallel_basic_1.json");
        let syndrome_vertices = vec![39, 52, 63, 90, 100];
        let half_weight = 500;
        primal_module_parallel_standard_syndrome(CodeCapacityPlanarCode::new(11, 0.1, half_weight), visualize_filename, syndrome_vertices, 9 * half_weight, |initializer, _config| {
            println!("initializer: {initializer:?}");
        }, None);
    }

    /// split into 2, with no syndrome vertex on the interface
    #[test]
    fn primal_module_parallel_basic_2() {  // cargo test primal_module_parallel_basic_2 -- --nocapture
        let visualize_filename = format!("primal_module_parallel_basic_2.json");
        let syndrome_vertices = vec![39, 52, 63, 90, 100];
        let half_weight = 500;
        primal_module_parallel_standard_syndrome(CodeCapacityPlanarCode::new(11, 0.1, half_weight), visualize_filename, syndrome_vertices, 9 * half_weight, |_initializer, config| {
            config.partitions = vec![
                VertexRange::new(0, 72),    // unit 0
                VertexRange::new(84, 132),  // unit 1
            ];
            config.fusions = vec![
                (0, 1),  // unit 2, by fusing 0 and 1
            ];
        }, None);
    }

    /// split into 2, with a syndrome vertex on the interface
    #[test]
    fn primal_module_parallel_basic_3() {  // cargo test primal_module_parallel_basic_3 -- --nocapture
        let visualize_filename = format!("primal_module_parallel_basic_3.json");
        let syndrome_vertices = vec![39, 52, 63, 90, 100];
        let half_weight = 500;
        primal_module_parallel_standard_syndrome(CodeCapacityPlanarCode::new(11, 0.1, half_weight), visualize_filename, syndrome_vertices, 9 * half_weight, |_initializer, config| {
            config.partitions = vec![
                VertexRange::new(0, 60),    // unit 0
                VertexRange::new(72, 132),  // unit 1
            ];
            config.fusions = vec![
                (0, 1),  // unit 2, by fusing 0 and 1
            ];
        }, None);
    }

    /// split into 4, with no syndrome vertex on the interface
    #[test]
    fn primal_module_parallel_basic_4() {  // cargo test primal_module_parallel_basic_4 -- --nocapture
        let visualize_filename = format!("primal_module_parallel_basic_4.json");
        // reorder vertices to enable the partition;
        let syndrome_vertices = vec![39, 52, 63, 90, 100];  // indices are before the reorder
        let half_weight = 500;
        primal_module_parallel_standard_syndrome(CodeCapacityPlanarCode::new(11, 0.1, half_weight), visualize_filename, syndrome_vertices, 9 * half_weight, |_initializer, config| {
            config.partitions = vec![
                VertexRange::new(0, 36),
                VertexRange::new(42, 72),
                VertexRange::new(84, 108),
                VertexRange::new(112, 132),
            ];
            config.fusions = vec![
                (0, 1),
                (2, 3),
                (4, 5),
            ];
        }, Some((|| {
            let mut reordered_vertices = vec![];
            let split_horizontal = 6;
            let split_vertical = 5;
            for i in 0..split_horizontal {  // left-top block
                for j in 0..split_vertical {
                    reordered_vertices.push(i * 12 + j);
                }
                reordered_vertices.push(i * 12 + 11);
            }
            for i in 0..split_horizontal {  // interface between the left-top block and the right-top block
                reordered_vertices.push(i * 12 + split_vertical);
            }
            for i in 0..split_horizontal {  // right-top block
                for j in (split_vertical+1)..10 {
                    reordered_vertices.push(i * 12 + j);
                }
                reordered_vertices.push(i * 12 + 10);
            }
            {  // the big interface between top and bottom
                for j in 0..12 {
                    reordered_vertices.push(split_horizontal * 12 + j);
                }
            }
            for i in (split_horizontal+1)..11 {  // left-bottom block
                for j in 0..split_vertical {
                    reordered_vertices.push(i * 12 + j);
                }
                reordered_vertices.push(i * 12 + 11);
            }
            for i in (split_horizontal+1)..11 {  // interface between the left-bottom block and the right-bottom block
                reordered_vertices.push(i * 12 + split_vertical);
            }
            for i in (split_horizontal+1)..11 {  // right-bottom block
                for j in (split_vertical+1)..10 {
                    reordered_vertices.push(i * 12 + j);
                }
                reordered_vertices.push(i * 12 + 10);
            }
            reordered_vertices
        })()));
    }

    /// split into 4, with 2 syndrome vertices on parent interfaces
    #[test]
    fn primal_module_parallel_basic_5() {  // cargo test primal_module_parallel_basic_5 -- --nocapture
        let visualize_filename = format!("primal_module_parallel_basic_5.json");
        // reorder vertices to enable the partition;
        let syndrome_vertices = vec![39, 52, 63, 90, 100];  // indices are before the reorder
        let half_weight = 500;
        primal_module_parallel_standard_syndrome(CodeCapacityPlanarCode::new(11, 0.1, half_weight), visualize_filename, syndrome_vertices, 9 * half_weight, |_initializer, config| {
            config.partitions = vec![
                VertexRange::new(0, 25),
                VertexRange::new(30, 60),
                VertexRange::new(72, 97),
                VertexRange::new(102, 132),
            ];
            config.fusions = vec![
                (0, 1),
                (2, 3),
                (4, 5),
            ];
        }, Some((|| {
            let mut reordered_vertices = vec![];
            let split_horizontal = 5;
            let split_vertical = 4;
            for i in 0..split_horizontal {  // left-top block
                for j in 0..split_vertical {
                    reordered_vertices.push(i * 12 + j);
                }
                reordered_vertices.push(i * 12 + 11);
            }
            for i in 0..split_horizontal {  // interface between the left-top block and the right-top block
                reordered_vertices.push(i * 12 + split_vertical);
            }
            for i in 0..split_horizontal {  // right-top block
                for j in (split_vertical+1)..10 {
                    reordered_vertices.push(i * 12 + j);
                }
                reordered_vertices.push(i * 12 + 10);
            }
            {  // the big interface between top and bottom
                for j in 0..12 {
                    reordered_vertices.push(split_horizontal * 12 + j);
                }
            }
            for i in (split_horizontal+1)..11 {  // left-bottom block
                for j in 0..split_vertical {
                    reordered_vertices.push(i * 12 + j);
                }
                reordered_vertices.push(i * 12 + 11);
            }
            for i in (split_horizontal+1)..11 {  // interface between the left-bottom block and the right-bottom block
                reordered_vertices.push(i * 12 + split_vertical);
            }
            for i in (split_horizontal+1)..11 {  // right-bottom block
                for j in (split_vertical+1)..10 {
                    reordered_vertices.push(i * 12 + j);
                }
                reordered_vertices.push(i * 12 + 10);
            }
            reordered_vertices
        })()));
    }

    fn primal_module_parallel_debug_planar_code_common(d: usize, visualize_filename: String, syndrome_vertices: Vec<VertexIndex>, final_dual: Weight) {
        let half_weight = 500;
        let split_horizontal = (d + 1) / 2;
        let row_count = d + 1;
        primal_module_parallel_standard_syndrome(CodeCapacityPlanarCode::new(d, 0.1, half_weight), visualize_filename, syndrome_vertices, final_dual * half_weight, |initializer, config| {
            config.partitions = vec![
                VertexRange::new(0, split_horizontal * row_count),
                VertexRange::new((split_horizontal + 1) * row_count, initializer.vertex_num),
            ];
            config.fusions = vec![
                (0, 1),
            ];
        }, None);
    }

    /// 68000 vs 69000 dual variable: probably missing some interface node
    /// panicked at 'vacating a non-boundary vertex is forbidden', src/dual_module_serial.rs:899:25
    /// reason: when executing sync events, I forgot to add the new propagated dual module to the active list;
    /// why it didn't show up before: because usually a node is created when executing sync event, in which case it's automatically in the active list
    /// if this node already exists before, and it's again synchronized, then it's not in the active list, leading to strange growth 
    #[test]
    fn primal_module_parallel_debug_1() {  // cargo test primal_module_parallel_debug_1 -- --nocapture
        let visualize_filename = format!("primal_module_parallel_debug_1.json");
        let syndrome_vertices = vec![88, 89, 102, 103, 105, 106, 118, 120, 122, 134, 138];  // indices are before the reorder
        primal_module_parallel_debug_planar_code_common(15, visualize_filename, syndrome_vertices, 10);
    }

}
