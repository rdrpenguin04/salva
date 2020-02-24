use crate::counters::Counters;
use crate::coupling::CouplingManager;
use crate::geometry::{self, ContactManager, HGrid, HGridEntry};
use crate::math::Vector;
use crate::object::{Boundary, BoundaryHandle, BoundarySet};
use crate::object::{Fluid, FluidHandle, FluidSet};
use crate::solver::PressureSolver;
use crate::TimestepManager;
use na::RealField;

#[cfg(feature = "parallel")]
use rayon::prelude::*;

/// The physics world for simulating fluids with boundaries.
pub struct LiquidWorld<N: RealField> {
    pub counters: Counters,
    nsubsteps_since_sort: usize,
    particle_radius: N,
    h: N,
    fluids: FluidSet<N>,
    boundaries: BoundarySet<N>,
    solver: Box<dyn PressureSolver<N>>,
    contact_manager: ContactManager<N>,
    timestep_manager: TimestepManager<N>,
    hgrid: HGrid<N, HGridEntry>,
}

impl<N: RealField> LiquidWorld<N> {
    /// Initialize a new liquid world.
    ///
    /// # Parameters
    ///
    /// - `particle_radius`: the radius of every particle on this world.
    /// - `smoothing_factor`: the smoothing factor used to compute the SPH kernel radius.
    ///    The kernel radius will be computed as `particle_radius * smoothing_factor * 2.0.
    pub fn new(
        solver: impl PressureSolver<N> + 'static,
        particle_radius: N,
        smoothing_factor: N,
    ) -> Self {
        let h = particle_radius * smoothing_factor * na::convert(2.0);
        Self {
            counters: Counters::new(false),
            nsubsteps_since_sort: 0,
            particle_radius,
            h,
            fluids: FluidSet::new(),
            boundaries: BoundarySet::new(),
            solver: Box::new(solver),
            contact_manager: ContactManager::new(),
            timestep_manager: TimestepManager::new(),
            hgrid: HGrid::new(h),
        }
    }

    /// Advances the simulation by `dt` milliseconds.
    ///
    /// All the fluid particles will be affected by an acceleration equal to `gravity`.
    pub fn step(&mut self, dt: N, gravity: &Vector<N>) {
        self.step_with_coupling(dt, gravity, &mut ())
    }

    /// Advances the simulation by `dt` milliseconds, taking into account coupling with an external rigid-body engine.
    pub fn step_with_coupling(
        &mut self,
        dt: N,
        gravity: &Vector<N>,
        coupling: &mut impl CouplingManager<N>,
    ) {
        self.counters.reset();
        self.counters.step_time.start();
        let mut remaining_time = dt;

        // Perform substeps.
        while remaining_time > N::zero() {
            self.nsubsteps_since_sort += 1;
            self.counters.nsubsteps += 1;

            // Substep length.
            let substep_dt = self.timestep_manager.compute_substep(
                dt,
                remaining_time,
                self.particle_radius,
                self.fluids.as_slice(),
            );

            self.solver.init_with_fluids(self.fluids.as_slice());
            self.solver
                .predict_advection(substep_dt, gravity, self.fluids.as_slice());

            self.counters.stages.collision_detection_time.resume();
            self.counters.cd.grid_insertion_time.resume();
            self.hgrid.clear();
            geometry::insert_fluids_to_grid(substep_dt, self.fluids.as_slice(), &mut self.hgrid);
            self.counters.cd.grid_insertion_time.pause();

            self.counters.cd.boundary_update_time.resume();
            coupling.update_boundaries(
                substep_dt,
                self.h,
                &self.hgrid,
                self.fluids.as_mut_slice(),
                self.solver.velocity_changes_mut(),
                &mut self.boundaries,
            );
            self.counters.cd.boundary_update_time.pause();

            self.counters.cd.grid_insertion_time.resume();
            geometry::insert_boundaries_to_grid(self.boundaries.as_slice(), &mut self.hgrid);
            self.counters.cd.grid_insertion_time.pause();

            self.solver.init_with_boundaries(self.boundaries.as_slice());

            self.contact_manager.update_contacts(
                &mut self.counters,
                self.h,
                self.fluids.as_slice(),
                self.boundaries.as_slice(),
                &self.hgrid,
            );

            self.counters.cd.ncontacts = self.contact_manager.ncontacts();
            self.counters.stages.collision_detection_time.pause();

            self.counters.stages.solver_time.resume();
            self.solver.step(
                &mut self.counters,
                substep_dt,
                &mut self.contact_manager,
                self.h,
                self.fluids.as_mut_slice(),
                self.boundaries.as_slice(),
            );

            coupling.transmit_forces(&self.boundaries);
            self.counters.stages.solver_time.pause();

            remaining_time -= substep_dt;
        }

        //        if self.nsubsteps_since_sort >= 100 {
        //            self.nsubsteps_since_sort = 0;
        //            println!("Performing z-sort of particles.");
        //            par_iter_mut!(self.fluids.as_mut_slice()).for_each(|fluid| fluid.z_sort())
        //        }

        self.counters.step_time.pause();
        println!("Counters: {}", self.counters);
    }

    /// Add a fluid to the liquid world.
    pub fn add_fluid(&mut self, fluid: Fluid<N>) -> FluidHandle {
        self.fluids.insert(fluid)
    }

    /// Add a boundary to the liquid world.
    pub fn add_boundary(&mut self, boundary: Boundary<N>) -> BoundaryHandle {
        self.boundaries.insert(boundary)
    }

    /// Add a fluid to the liquid world.
    pub fn remove_fluid(&mut self, handle: FluidHandle) -> Option<Fluid<N>> {
        self.fluids.remove(handle)
    }

    /// Add a boundary to the liquid world.
    pub fn remove_boundary(&mut self, handle: BoundaryHandle) -> Option<Boundary<N>> {
        self.boundaries.remove(handle)
    }

    /// The set of fluids on this liquid world.
    pub fn fluids(&self) -> &FluidSet<N> {
        &self.fluids
    }

    /// The set of boundaries on this liquid world.
    pub fn boundaries(&self) -> &BoundarySet<N> {
        &self.boundaries
    }

    /// The SPH kernel radius.
    pub fn h(&self) -> N {
        self.h
    }

    /// The radius of every particle on this liquid world.
    pub fn particle_radius(&self) -> N {
        self.particle_radius
    }
}
