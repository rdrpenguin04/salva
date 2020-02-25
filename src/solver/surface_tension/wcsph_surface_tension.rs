#[cfg(feature = "parallel")]
use rayon::prelude::*;

use na::{self, RealField};

use crate::geometry::ParticlesContacts;

use crate::math::Vector;
use crate::object::Fluid;
use crate::solver::NonPressureForce;
use crate::TimestepManager;

// Surface tension of water: 0.01
// Stable values of surface tension: up to 3.4
// From https://cg.informatik.uni-freiburg.de/publications/2007_SCA_SPH.pdf
#[derive(Clone)]
pub struct WCSPHSurfaceTension<N: RealField> {
    tension_coefficient: N,
}

impl<N: RealField> WCSPHSurfaceTension<N> {
    pub fn new(tension_coefficient: N) -> Self {
        Self {
            tension_coefficient,
        }
    }
}

impl<N: RealField> NonPressureForce<N> for WCSPHSurfaceTension<N> {
    fn solve(
        &mut self,
        timestep: &TimestepManager<N>,
        _kernel_radius: N,
        fluid_fluid_contacts: &ParticlesContacts<N>,
        fluid: &mut Fluid<N>,
        _densities: &[N],
    ) {
        let tension_coefficient = self.tension_coefficient;
        let positions = &fluid.positions;
        let volumes = &fluid.volumes;
        let density0 = fluid.density0;

        par_iter_mut!(fluid.accelerations)
            .enumerate()
            .for_each(|(i, acceleration_i)| {
                for c in fluid_fluid_contacts
                    .particle_contacts(i)
                    .read()
                    .unwrap()
                    .iter()
                {
                    if c.i_model == c.j_model {
                        let dpos = positions[c.i] - positions[c.j];
                        let cohesion_acc = dpos
                            * (-tension_coefficient * c.weight * volumes[c.j] * density0
                                / (volumes[c.i] * density0));
                        *acceleration_i += cohesion_acc;
                    }
                }
            })
    }

    fn apply_permutation(&mut self, _: &[usize]) {}
}
