use crate::pairing::ff::{Field, PrimeField};
use crate::pairing::{Engine};
use crate::multicore::*;

use crate::{SynthesisError};
use std::marker::PhantomData;
use super::cs::*;
use super::gates::*;
use super::data_structures::*;
use super::utils::*;

use crate::plonk::polynomials::*;
use crate::plonk::transparent_engine::PartialTwoBitReductionField;
use crate::plonk::commitments::transparent::fri::coset_combining_fri::*;
use crate::plonk::commitments::transparent::fri::coset_combining_fri::fri::*;
use crate::plonk::commitments::transparent::iop_compiler::*;
use crate::plonk::commitments::transparent::iop_compiler::coset_combining_blake2s_tree::*;
use crate::plonk::commitments::transcript::*;
use crate::plonk::fft::cooley_tukey_ntt::*;

#[derive(Debug)]
pub(crate) struct ProvingAssembly<E: Engine> {
    m: usize,
    n: usize,
    input_gates: Vec<Gate<E::Fr>>,
    aux_gates: Vec<Gate<E::Fr>>,

    num_inputs: usize,
    num_aux: usize,

    input_assingments: Vec<E::Fr>,
    aux_assingments: Vec<E::Fr>,

    inputs_map: Vec<usize>,

    is_finalized: bool
}

impl<E: Engine> ConstraintSystem<E> for ProvingAssembly<E> {
    // const ZERO: Variable = Variable(Index::Aux(1));
    // const ONE: Variable = Variable(Index::Aux(2));

    // allocate a variable
    fn alloc<F>(&mut self, value: F) -> Result<Variable, SynthesisError>
    where
        F: FnOnce() -> Result<E::Fr, SynthesisError> 
    {
        let value = value()?;

        self.num_aux += 1;
        let index = self.num_aux;
        self.aux_assingments.push(value);

        Ok(Variable(Index::Aux(index)))
    }

    // allocate an input variable
    fn alloc_input<F>(&mut self, value: F) -> Result<Variable, SynthesisError>
    where
        F: FnOnce() -> Result<E::Fr, SynthesisError> 
    {
        let value = value()?;

        self.num_inputs += 1;
        let index = self.num_inputs;
        self.input_assingments.push(value);

        let input_var = Variable(Index::Input(index));

        let gate = Gate::<E::Fr>::new_enforce_constant_gate(input_var, Some(E::Fr::zero()), self.dummy_variable());
        self.input_gates.push(gate);

        Ok(input_var)

    }

    fn get_dummy_variable(&self) -> Variable {
        self.dummy_variable()
    }

    // allocate an abstract gate
    fn new_gate(&mut self, variables: (Variable, Variable, Variable), 
        coeffs: (E::Fr, E::Fr, E::Fr, E::Fr, E::Fr, E::Fr)) -> Result<(), SynthesisError>
    {
        let gate = Gate::<E::Fr>::new_gate(variables, coeffs);
        self.aux_gates.push(gate);
        self.n += 1;

        Ok(())
    }

    fn get_value(&self, var: Variable) -> Result<E::Fr, SynthesisError> {
        let value = match var {
            Variable(Index::Aux(0)) => {
                E::Fr::zero()
            }
            Variable(Index::Input(0)) => {
                return Err(SynthesisError::AssignmentMissing);
            }
            Variable(Index::Input(input)) => {
                self.input_assingments[input - 1]
            },
            Variable(Index::Aux(aux)) => {
                self.aux_assingments[aux - 1]
            }
        };

        Ok(value)
    }
}

impl<E: Engine> ProvingAssembly<E> {
    // allocate a constant
    fn enforce_constant(&mut self, variable: Variable, constant: E::Fr) -> Result<(), SynthesisError>
    {
        let gate = Gate::<E::Fr>::new_enforce_constant_gate(variable, Some(constant), self.dummy_variable());
        self.aux_gates.push(gate);
        self.n += 1;

        Ok(())
    }

    pub(crate) fn new() -> Self {
        let mut tmp = Self {
            n: 0,
            m: 0,
            input_gates: vec![],
            aux_gates: vec![],

            num_inputs: 0,
            num_aux: 0,

            input_assingments: vec![],
            aux_assingments: vec![],

            inputs_map: vec![],

            is_finalized: false,
        };

        let zero = tmp.alloc(|| Ok(E::Fr::zero())).expect("should have no issues");
        tmp.enforce_constant(zero, E::Fr::zero()).expect("should have no issues");

        match (tmp.dummy_variable(), zero) {
            (Variable(Index::Aux(1)), Variable(Index::Aux(1))) => {},
            _ => panic!("zero variable is incorrect")
        }

        tmp
    }

     fn new_empty_gate(&mut self) -> usize {
        self.n += 1;
        let index = self.n;

        self.aux_gates.push(Gate::<E::Fr>::empty());

        index
    }

    fn set_gate(&mut self, gate: Gate<E::Fr>, index: usize) {
        self.aux_gates[index-1] = gate;
    }

    // return variable that is not in a constraint formally, but has some value
    fn dummy_variable(&self) -> Variable {
        // <Self as ConstraintSystem<E>>::ZERO
        Variable(Index::Aux(1))
    }

    pub(crate) fn num_gates(&self) -> usize {
        assert!(self.is_finalized);

        self.input_gates.len() + self.aux_gates.len()
    }

    fn finalize(&mut self) {
        if !self.is_finalized {
            let n = self.input_gates.len() + self.aux_gates.len();
            if !(n+1).is_power_of_two() {
                let empty_gate = Gate::<E::Fr>::new_empty_gate(self.dummy_variable());
                let new_aux_len = (n+1).next_power_of_two() - 1 - self.input_gates.len();

                self.aux_gates.resize(new_aux_len, empty_gate);
            }
            self.is_finalized = true;
        }
    }

    fn get_data(&self) -> (&Vec<Gate<E::Fr>>, &Vec<Gate<E::Fr>>, &usize, &usize) {
        (&self.input_gates, &self.aux_gates, &self.num_inputs, &self.num_aux)
    }

    pub(crate) fn make_wire_assingments(&self) -> (Vec<E::Fr>, Vec<E::Fr>, Vec<E::Fr>) {
        assert!(self.is_finalized);
        // create a vector of gate assingments
        // if a_i = j then w_j = f_l(g^i)

        let total_num_gates = self.input_gates.len() + self.aux_gates.len();
        let mut f_l = vec![E::Fr::zero(); total_num_gates];
        let mut f_r = vec![E::Fr::zero(); total_num_gates];
        let mut f_o = vec![E::Fr::zero(); total_num_gates];

        for (i, gate) in self.input_gates.iter().chain(&self.aux_gates).enumerate()
        {
            match gate.a_wire() {
                Variable(Index::Input(index)) => {
                    f_l[i] = self.input_assingments[index - 1];
                },
                Variable(Index::Aux(index)) => {
                    f_l[i] = self.aux_assingments[index - 1];
                },
            }

            match gate.b_wire() {
                Variable(Index::Input(index)) => {
                    f_r[i] = self.input_assingments[index - 1];
                },
                Variable(Index::Aux(index)) => {
                    f_r[i] = self.aux_assingments[index - 1];
                },
            }

            match gate.c_wire() {
                Variable(Index::Input(index)) => {
                    f_o[i] = self.input_assingments[index - 1];
                },
                Variable(Index::Aux(index)) => {
                    f_o[i] = self.aux_assingments[index - 1];
                },
            }
        }

        (f_l, f_r, f_o)
    }
}

fn prove_with_setup_precomputed<E: Engine, C: Circuit<E>, CP: CTPrecomputations<E::Fr>, CPI: CTPrecomputations<E::Fr>, 
    FP: FriPrecomputations<E::Fr>, T: Transcript<E::Fr, 
    Input = <FriSpecificBlake2sTree<E::Fr> as IopInstance<E::Fr>> :: Commitment> >
(
    circuit: &C,
    setup_precomp: &RedshiftSetupPrecomputation<E::Fr, FriSpecificBlake2sTree<E::Fr>>,
    params: &RedshiftParameters<E::Fr>,
    omegas_bitreversed: &CP,
    omegas_inv_bitreversed: &CPI,
    bitreversed_omegas_for_fri: &FP      
) -> Result<(), SynthesisError> 
where E::Fr : PartialTwoBitReductionField
{
    let mut assembly = ProvingAssembly::<E>::new();
    circuit.synthesize(&mut assembly)?;
    assembly.finalize();
    let (input_gates, aux_gates, num_inputs, num_aux) = assembly.get_data();
    
    let n = input_gates.len() + aux_gates.len();
    let worker = Worker::new();
    let mut transcript = T::new();

    // we need n+1 to be a power of two and can not have n to be power of two
    let required_domain_size = n + 1;
    assert!(required_domain_size.is_power_of_two());

    let (w_l, w_r, w_o) = assembly.make_wire_assingments();

    // these are 2^k - 1 size and explicitly unpadded
    let w_l = Polynomial::<E::Fr, Values>::from_values_unpadded(w_l)?;
    let w_r = Polynomial::<E::Fr, Values>::from_values_unpadded(w_r)?;
    let w_o = Polynomial::<E::Fr, Values>::from_values_unpadded(w_o)?;

    let a_poly = w_l.clone_padded_to_domain()?.ifft_using_bitreversed_ntt_with_partial_reduction(&worker, omegas_inv_bitreversed, &E::Fr::one())?;
    let b_poly = w_r.clone_padded_to_domain()?.ifft_using_bitreversed_ntt_with_partial_reduction(&worker, omegas_inv_bitreversed, &E::Fr::one())?;
    let c_poly = w_o.clone_padded_to_domain()?.ifft_using_bitreversed_ntt_with_partial_reduction(&worker, omegas_inv_bitreversed, &E::Fr::one())?;

    // polynomials inside of these is are values in cosets

    let a_commitment_data = commit_single_poly::<E, CP>(&a_poly, omegas_bitreversed, &params, &worker)?;
    let b_commitment_data = commit_single_poly::<E, CP>(&b_poly, omegas_bitreversed, &params, &worker)?;
    let c_commitment_data = commit_single_poly::<E, CP>(&c_poly, omegas_bitreversed, &params, &worker)?;

    transcript.commit_input(&a_commitment_data.oracle.get_commitment());
    transcript.commit_input(&b_commitment_data.oracle.get_commitment());
    transcript.commit_input(&c_commitment_data.oracle.get_commitment());

    // TODO: Add public inputs

    let beta = transcript.get_challenge();
    let gamma = transcript.get_challenge();

    let mut w_l_plus_gamma = w_l.clone();
    w_l_plus_gamma.add_constant(&worker, &gamma);

    let mut w_r_plus_gamma = w_r.clone();
    w_r_plus_gamma.add_constant(&worker, &gamma);

    let mut w_o_plus_gamma = w_o.clone();
    w_o_plus_gamma.add_constant(&worker, &gamma);

    // we take A, B, C values and form (A + beta*i + gamma), etc and calculate their grand product

    let z_1 = {
        let s_id_1: Vec<_> = (1..=n).collect();
        let s_id_1 = convert_to_field_elements(&s_id_1, &worker);
        let s_id_1 = Polynomial::<E::Fr, Values>::from_values_unpadded(s_id_1)?;
        let mut w_l_contribution = w_l_plus_gamma.clone();
        w_l_contribution.add_assign_scaled(&worker, &s_id_1, &beta);
        drop(s_id_1);

        let s_id_2: Vec<_> = ((n+1)..=(2*n)).collect();
        let s_id_2 = convert_to_field_elements(&s_id_2, &worker);
        let s_id_2 = Polynomial::<E::Fr, Values>::from_values_unpadded(s_id_2)?;
        let mut w_r_contribution = w_r_plus_gamma.clone();
        w_r_contribution.add_assign_scaled(&worker, &s_id_2, &beta);
        drop(s_id_2);
        w_l_contribution.mul_assign(&worker, &w_r_contribution);
        drop(w_r_contribution);

        let s_id_3: Vec<_> = ((2*n+1)..=(3*n)).collect();
        let s_id_3 = convert_to_field_elements(&s_id_3, &worker);
        let s_id_3 = Polynomial::<E::Fr, Values>::from_values_unpadded(s_id_3)?;
        let mut w_o_contribution = w_o_plus_gamma.clone();
        w_o_contribution.add_assign_scaled(&worker, &s_id_3, &beta);
        drop(s_id_3);
        w_l_contribution.mul_assign(&worker, &w_o_contribution);
        drop(w_o_contribution);

        let grand_product = w_l_contribution.calculate_grand_product(&worker)?;

        drop(w_l_contribution);

        let values = grand_product.into_coeffs();
        assert!((values.len() + 1).is_power_of_two());
        let mut prepadded = Vec::with_capacity(values.len() + 1);
        prepadded.push(E::Fr::one());
        prepadded.extend(values);

        Polynomial::<E::Fr, Values>::from_values(prepadded)?
    };

    let z_2 = {
        let (sigma_1, sigma_2, sigma_3) = 
            calculate_permutations_as_in_a_paper::<E>(input_gates, aux_gates, num_inputs, num_aux);

        let sigma_1 = convert_to_field_elements(&sigma_1, &worker);
        let sigma_1 = Polynomial::<E::Fr, Values>::from_values_unpadded(sigma_1)?;
        let mut w_l_contribution = w_l_plus_gamma.clone();
        w_l_contribution.add_assign_scaled(&worker, &sigma_1, &beta);
        drop(sigma_1);

        let sigma_2 = convert_to_field_elements(&sigma_2, &worker);
        let sigma_2 = Polynomial::<E::Fr, Values>::from_values_unpadded(sigma_2)?;
        let mut w_r_contribution = w_r_plus_gamma.clone();
        w_r_contribution.add_assign_scaled(&worker, &sigma_2, &beta);
        drop(sigma_2);
        w_l_contribution.mul_assign(&worker, &w_r_contribution);
        drop(w_r_contribution);

        let sigma_3 = convert_to_field_elements(&sigma_3, &worker);
        let sigma_3 = Polynomial::<E::Fr, Values>::from_values_unpadded(sigma_3)?;
        let mut w_o_contribution = w_o_plus_gamma.clone();
        w_o_contribution.add_assign_scaled(&worker, &sigma_3, &beta);
        drop(sigma_3);
        w_l_contribution.mul_assign(&worker, &w_o_contribution);
        drop(w_o_contribution);

        let grand_product = w_l_contribution.calculate_grand_product(&worker)?;

        drop(w_l_contribution);

        let values = grand_product.into_coeffs();
        assert!((values.len() + 1).is_power_of_two());
        let mut prepadded = Vec::with_capacity(values.len() + 1);
        prepadded.push(E::Fr::one());
        prepadded.extend(values);

        let z_2 = Polynomial::<E::Fr, Values>::from_values(prepadded)?;

        z_2
    };

    assert!(z_2.as_ref().last().expect("must exist") == z_1.as_ref().last().expect("must exist"));

    // interpolate on the main domain
    let z_1 = z_1.ifft_using_bitreversed_ntt_with_partial_reduction(&worker, omegas_inv_bitreversed, &E::Fr::one())?;
    let z_2 = z_2.ifft_using_bitreversed_ntt_with_partial_reduction(&worker, omegas_inv_bitreversed, &E::Fr::one())?;

    // polynomials inside of these is are values in cosets

    let z_1_commitment_data = commit_single_poly::<E, CP>(&z_1, omegas_bitreversed, &params, &worker)?;
    let z_2_commitment_data = commit_single_poly::<E, CP>(&z_2, omegas_bitreversed, &params, &worker)?;

    transcript.commit_input(&z_1_commitment_data.oracle.get_commitment());
    transcript.commit_input(&z_2_commitment_data.oracle.get_commitment());

    let mut z_1_shifted = z_1.clone();
    z_1_shifted.distribute_powers(&worker, z_1.omega);
    
    let mut z_2_shifted = z_2.clone();
    z_2_shifted.distribute_powers(&worker, z_2.omega);

    let partition_factor = params.lde_factor / 4;

    assert!(partition_factor > 0);
    assert!(partition_factor.is_power_of_two());

    let a_coset_lde_bitreversed = a_commitment_data.poly.clone_subset_assuming_bitreversed(partition_factor)?;
    let b_coset_lde_bitreversed = b_commitment_data.poly.clone_subset_assuming_bitreversed(partition_factor)?;
    let c_coset_lde_bitreversed = c_commitment_data.poly.clone_subset_assuming_bitreversed(partition_factor)?;
    
    let q_l_coset_lde_bitreversed = setup_precomp.q_l_aux.poly.clone_subset_assuming_bitreversed(partition_factor)?;
    let q_r_coset_lde_bitreversed = setup_precomp.q_r_aux.poly.clone_subset_assuming_bitreversed(partition_factor)?;
    let q_o_coset_lde_bitreversed = setup_precomp.q_o_aux.poly.clone_subset_assuming_bitreversed(partition_factor)?;
    let q_m_coset_lde_bitreversed = setup_precomp.q_m_aux.poly.clone_subset_assuming_bitreversed(partition_factor)?;
    let q_c_coset_lde_bitreversed = setup_precomp.q_c_aux.poly.clone_subset_assuming_bitreversed(partition_factor)?;
    //let q_add_sel_coset_lde_bitreversed = setup_precomp.q_add_sel_aux.poly.clone_subset_assuming_bitreversed(partition_factor)?;
    let s_id_coset_lde_bitreversed = setup_precomp.s_id_aux.poly.clone_subset_assuming_bitreversed(partition_factor)?;
    let sigma_1_coset_lde_bitreversed = setup_precomp.sigma_1_aux.poly.clone_subset_assuming_bitreversed(partition_factor)?;
    let sigma_2_coset_lde_bitreversed = setup_precomp.sigma_2_aux.poly.clone_subset_assuming_bitreversed(partition_factor)?;
    let sigma_3_coset_lde_bitreversed = setup_precomp.sigma_3_aux.poly.clone_subset_assuming_bitreversed(partition_factor)?;

    let (q_l, q_r, q_o, q_m, q_c, q_add_sel, s_id, sigma_1, sigma_2, sigma_3) = 
        output_setup_polynomials::<E>(input_gates, aux_gates, num_inputs, num_aux, &worker)?;

    // we do not commit those cause those are known already

    let n_fe = E::Fr::from_str(&n.to_string()).expect("must be valid field element");
    let mut two_n_fe = n_fe;
    two_n_fe.double();

    let alpha = transcript.get_challenge();

    // TODO: may be speedup this one too
    let mut vanishing_poly_inverse_bitreversed = 
        calculate_inverse_vanishing_polynomial_in_a_coset::<E>(&worker, q_l_coset_lde_bitreversed.size(), required_domain_size.next_power_of_two())?;
    vanishing_poly_inverse_bitreversed.bitreverse_enumeration(&worker);

    let mut t_1 = {
        let mut t_1 = q_c_coset_lde_bitreversed;

        let mut q_l_by_a = q_l_coset_lde_bitreversed;
        q_l_by_a.mul_assign(&worker, &a_coset_lde_bitreversed);
        t_1.add_assign(&worker, &q_l_by_a);
        drop(q_l_by_a);

        let mut q_r_by_b = q_r_coset_lde_bitreversed;
        q_r_by_b.mul_assign(&worker, &b_coset_lde_bitreversed);
        t_1.add_assign(&worker, &q_r_by_b);
        drop(q_r_by_b);

        let mut q_o_by_c = q_o_coset_lde_bitreversed;
        q_o_by_c.mul_assign(&worker, &c_coset_lde_bitreversed);
        t_1.add_assign(&worker, &q_o_by_c);
        drop(q_o_by_c);

        let mut q_m_by_ab = q_m_coset_lde_bitreversed;
        q_m_by_ab.mul_assign(&worker, &a_coset_lde_bitreversed);
        q_m_by_ab.mul_assign(&worker, &b_coset_lde_bitreversed);
        t_1.add_assign(&worker, &q_m_by_ab);
        drop(q_m_by_ab);

        //let mut q_add_sel_by_c_next = q_add_sel_coset_lde_bitreversed;

        vanishing_poly_inverse_bitreversed.scale(&worker, alpha);

        t_1.mul_assign(&worker, &vanishing_poly_inverse_bitreversed);

        t_1
    };

    fn get_degree<F: PrimeField>(poly: &Polynomial<F, Coefficients>) -> usize {
        let mut degree = poly.as_ref().len() - 1;
        for c in poly.as_ref().iter().rev() {
            if c.is_zero() {
                degree -= 1;
            } else {
                break;
            }
        }

        println!("Degree = {}", degree);

        degree
    }

    let z_1_coset_lde_bitreversed = z_1_commitment_data.poly.clone_subset_assuming_bitreversed(partition_factor)?;

    assert!(z_1_coset_lde_bitreversed.size() == required_domain_size*4);

    let z_1_shifted_coset_lde_bitreversed = z_1_shifted.clone().bitreversed_lde_using_bitreversed_ntt_with_partial_reduction(
        &worker, 
        4, 
        omegas_bitreversed, 
        &E::Fr::multiplicative_generator()
    )?;

    assert!(z_1_shifted_coset_lde_bitreversed.size() == required_domain_size*4);

    let z_2_coset_lde_bitreversed = z_2_commitment_data.poly.clone_subset_assuming_bitreversed(partition_factor)?;

    assert!(z_2_coset_lde_bitreversed.size() == required_domain_size*4);

    let z_2_shifted_coset_lde_bitreversed = z_2_shifted.clone().bitreversed_lde_using_bitreversed_ntt_with_partial_reduction(
        &worker, 
        4, 
        omegas_bitreversed, 
        &E::Fr::multiplicative_generator()
    )?;

    assert!(z_2_shifted_coset_lde_bitreversed.size() == required_domain_size*4);

    // (A + beta*i + gamma)(B + beta(n+i) + gamma)(C + beta(2n+i) + gamma)*Z(k) = Z(k+1)
    {
        // TODO: May be optimize number of additions
        let mut contrib_z_1 = z_1_coset_lde_bitreversed.clone();

        let mut s_id_by_beta = s_id_coset_lde_bitreversed;
        s_id_by_beta.scale(&worker, beta);

        let mut n_by_beta = n_fe;
        n_by_beta.mul_assign(&beta);

        let mut a_perm = s_id_by_beta.clone();
        a_perm.add_constant(&worker, &gamma);
        a_perm.add_assign(&worker, &a_coset_lde_bitreversed);
        contrib_z_1.mul_assign(&worker, &a_perm);
        drop(a_perm);

        s_id_by_beta.add_constant(&worker, &n_by_beta);

        let mut b_perm = s_id_by_beta.clone();

        b_perm.add_constant(&worker, &gamma);
        b_perm.add_assign(&worker, &b_coset_lde_bitreversed);
        contrib_z_1.mul_assign(&worker, &b_perm);
        drop(b_perm);

        s_id_by_beta.add_constant(&worker, &n_by_beta);

        let mut c_perm = s_id_by_beta;
        c_perm.add_constant(&worker, &gamma);
        c_perm.add_assign(&worker, &c_coset_lde_bitreversed);
        contrib_z_1.mul_assign(&worker, &c_perm);
        drop(c_perm);

        contrib_z_1.sub_assign(&worker, &z_1_shifted_coset_lde_bitreversed);

        vanishing_poly_inverse_bitreversed.scale(&worker, alpha);

        contrib_z_1.mul_assign(&worker, &vanishing_poly_inverse_bitreversed);

        t_1.add_assign(&worker, &contrib_z_1);
    }

    {
        // TODO: May be optimize number of additions
        let mut contrib_z_2 = z_2_coset_lde_bitreversed.clone();

        let mut a_perm = sigma_1_coset_lde_bitreversed;
        a_perm.scale(&worker, beta);
        a_perm.add_constant(&worker, &gamma);
        a_perm.add_assign(&worker, &a_coset_lde_bitreversed);
        contrib_z_2.mul_assign(&worker, &a_perm);
        drop(a_perm);

        let mut b_perm = sigma_2_coset_lde_bitreversed;
        b_perm.scale(&worker, beta);
        b_perm.add_constant(&worker, &gamma);
        b_perm.add_assign(&worker, &b_coset_lde_bitreversed);
        contrib_z_2.mul_assign(&worker, &b_perm);
        drop(b_perm);

        let mut c_perm = sigma_3_coset_lde_bitreversed;
        c_perm.scale(&worker, beta);
        c_perm.add_constant(&worker, &gamma);
        c_perm.add_assign(&worker, &c_coset_lde_bitreversed);
        contrib_z_2.mul_assign(&worker, &c_perm);
        drop(c_perm);

        contrib_z_2.sub_assign(&worker, &z_2_shifted_coset_lde_bitreversed);

        vanishing_poly_inverse_bitreversed.scale(&worker, alpha);

        contrib_z_2.mul_assign(&worker, &vanishing_poly_inverse_bitreversed);

        t_1.add_assign(&worker, &contrib_z_2);
    }

    drop(a_coset_lde_bitreversed);
    drop(b_coset_lde_bitreversed);
    drop(c_coset_lde_bitreversed);

    let l_0 = calculate_lagrange_poly::<E>(&worker, required_domain_size.next_power_of_two(), 0)?;
    let l_n_minus_one = calculate_lagrange_poly::<E>(&worker, required_domain_size.next_power_of_two(), n-1)?;

    {
        let mut z_1_minus_z_2_shifted = z_1_shifted_coset_lde_bitreversed.clone();
        z_1_minus_z_2_shifted.sub_assign(&worker, &z_2_shifted_coset_lde_bitreversed);

        let l_coset_lde_bitreversed = l_n_minus_one.clone().bitreversed_lde_using_bitreversed_ntt_with_partial_reduction(
            &worker, 
            4, 
            omegas_bitreversed, 
            &E::Fr::multiplicative_generator()
        )?;

        z_1_minus_z_2_shifted.mul_assign(&worker, &l_coset_lde_bitreversed);
        drop(l_coset_lde_bitreversed);

        vanishing_poly_inverse_bitreversed.scale(&worker, alpha);

        z_1_minus_z_2_shifted.mul_assign(&worker, &vanishing_poly_inverse_bitreversed);

        t_1.add_assign(&worker, &z_1_minus_z_2_shifted);
    }

    {
        let mut z_1_minus_z_2 = z_1_coset_lde_bitreversed.clone();
        z_1_minus_z_2.sub_assign(&worker, &z_2_coset_lde_bitreversed);

        let l_coset_lde_bitreversed = l_0.clone().bitreversed_lde_using_bitreversed_ntt_with_partial_reduction(
            &worker, 
            4, 
            omegas_bitreversed, 
            &E::Fr::multiplicative_generator()
        )?;

        z_1_minus_z_2.mul_assign(&worker, &l_coset_lde_bitreversed);
        drop(l_coset_lde_bitreversed);

        vanishing_poly_inverse_bitreversed.scale(&worker, alpha);

        z_1_minus_z_2.mul_assign(&worker, &vanishing_poly_inverse_bitreversed);

        t_1.add_assign(&worker, &z_1_minus_z_2);
    }

    drop(z_1_coset_lde_bitreversed);
    drop(z_2_coset_lde_bitreversed);
    drop(z_1_shifted_coset_lde_bitreversed);
    drop(z_2_shifted_coset_lde_bitreversed);

    t_1.bitreverse_enumeration(&worker);

    let t_poly = t_1.icoset_fft_for_generator(&worker, &E::Fr::multiplicative_generator());

    debug_assert!(get_degree::<E::Fr>(&t_poly) <= 3*n);

    let mut t_poly_parts = t_poly.break_into_multiples(required_domain_size)?;

    t_poly_parts.pop().expect("last part is irrelevant");
    let t_poly_high = t_poly_parts.pop().expect("high exists");
    let t_poly_mid = t_poly_parts.pop().expect("mid exists");
    let t_poly_low = t_poly_parts.pop().expect("low exists");

    let t_poly_high_commitment_data = commit_single_poly::<E, CP>(&t_poly_high, omegas_bitreversed, &params, &worker)?;
    let t_poly_mid_commitment_data = commit_single_poly::<E, CP>(&t_poly_mid, omegas_bitreversed, &params, &worker)?;
    let t_poly_low_commitment_data = commit_single_poly::<E, CP>(&t_poly_low, omegas_bitreversed, &params, &worker)?;

    transcript.commit_input(&t_poly_low_commitment_data.oracle.get_commitment());
    transcript.commit_input(&t_poly_mid_commitment_data.oracle.get_commitment());
    transcript.commit_input(&t_poly_high_commitment_data.oracle.get_commitment());

    let z = transcript.get_challenge();

    let a_at_z = a_poly.evaluate_at(&worker, z);
    let b_at_z = b_poly.evaluate_at(&worker, z);
    let c_at_z = c_poly.evaluate_at(&worker, z);

    let q_l_at_z = q_l.evaluate_at(&worker, z);
    let q_r_at_z = q_r.evaluate_at(&worker, z);
    let q_o_at_z = q_o.evaluate_at(&worker, z);
    let q_m_at_z = q_m.evaluate_at(&worker, z);
    let q_c_at_z = q_c.evaluate_at(&worker, z);

    let s_id_at_z = s_id.evaluate_at(&worker, z);
    let sigma_1_at_z = sigma_1.evaluate_at(&worker, z);
    let sigma_2_at_z = sigma_2.evaluate_at(&worker, z);
    let sigma_3_at_z = sigma_3.evaluate_at(&worker, z);

    let mut inverse_vanishing_at_z = evaluate_inverse_vanishing_poly::<E>(required_domain_size.next_power_of_two(), z);

    let z_1_at_z = z_1.evaluate_at(&worker, z);
    let z_2_at_z = z_2.evaluate_at(&worker, z);

    let z_1_shifted_at_z = z_1_shifted.evaluate_at(&worker, z);
    let z_2_shifted_at_z = z_2_shifted.evaluate_at(&worker, z);

    let t_low_at_z = t_poly_low.evaluate_at(&worker, z);
    let t_mid_at_z = t_poly_mid.evaluate_at(&worker, z);
    let t_high_at_z = t_poly_high.evaluate_at(&worker, z);

    let l_0_at_z = l_0.evaluate_at(&worker, z);
    let l_n_minus_one_at_z = l_n_minus_one.evaluate_at(&worker, z);

    {
        transcript.commit_field_element(&a_at_z);
        transcript.commit_field_element(&b_at_z);
        transcript.commit_field_element(&c_at_z);

        transcript.commit_field_element(&q_l_at_z);
        transcript.commit_field_element(&q_r_at_z);
        transcript.commit_field_element(&q_o_at_z);
        transcript.commit_field_element(&q_m_at_z);
        transcript.commit_field_element(&q_c_at_z);

        transcript.commit_field_element(&s_id_at_z);
        transcript.commit_field_element(&sigma_1_at_z);
        transcript.commit_field_element(&sigma_2_at_z);
        transcript.commit_field_element(&sigma_3_at_z);

        transcript.commit_field_element(&t_low_at_z);
        transcript.commit_field_element(&t_mid_at_z);
        transcript.commit_field_element(&t_high_at_z);

        transcript.commit_field_element(&z_1_at_z);
        transcript.commit_field_element(&z_2_at_z);

        transcript.commit_field_element(&z_1_shifted_at_z);
        transcript.commit_field_element(&z_2_shifted_at_z);
    }

    // let aggregation_challenge = transcript.get_challenge();

    let z_in_pow_of_domain_size = z.pow([required_domain_size as u64]);

    // this is a sanity check
    {
        let mut t_1 = {
            let mut res = q_c_at_z;

            let mut tmp = q_l_at_z;
            tmp.mul_assign(&a_at_z);
            res.add_assign(&tmp);

            let mut tmp = q_r_at_z;
            tmp.mul_assign(&b_at_z);
            res.add_assign(&tmp);

            let mut tmp = q_o_at_z;
            tmp.mul_assign(&c_at_z);
            res.add_assign(&tmp);

            let mut tmp = q_m_at_z;
            tmp.mul_assign(&a_at_z);
            tmp.mul_assign(&b_at_z);
            res.add_assign(&tmp);

            inverse_vanishing_at_z.mul_assign(&alpha);

            res.mul_assign(&inverse_vanishing_at_z);

            res
        };

        {
            let mut res = z_1_at_z;

            let mut tmp = s_id_at_z;
            tmp.mul_assign(&beta);
            tmp.add_assign(&a_at_z);
            tmp.add_assign(&gamma);
            res.mul_assign(&tmp);

            let mut tmp = s_id_at_z;
            tmp.add_assign(&n_fe);
            tmp.mul_assign(&beta);
            tmp.add_assign(&b_at_z);
            tmp.add_assign(&gamma);
            res.mul_assign(&tmp);

            let mut tmp = s_id_at_z;
            tmp.add_assign(&two_n_fe);
            tmp.mul_assign(&beta);
            tmp.add_assign(&c_at_z);
            tmp.add_assign(&gamma);
            res.mul_assign(&tmp);

            res.sub_assign(&z_1_shifted_at_z);

            inverse_vanishing_at_z.mul_assign(&alpha);

            res.mul_assign(&inverse_vanishing_at_z);

            t_1.add_assign(&res);
        }

        {
            let mut res = z_2_at_z;

            let mut tmp = sigma_1_at_z;
            tmp.mul_assign(&beta);
            tmp.add_assign(&a_at_z);
            tmp.add_assign(&gamma);
            res.mul_assign(&tmp);

            let mut tmp = sigma_2_at_z;
            tmp.mul_assign(&beta);
            tmp.add_assign(&b_at_z);
            tmp.add_assign(&gamma);
            res.mul_assign(&tmp);

            let mut tmp = sigma_3_at_z;
            tmp.mul_assign(&beta);
            tmp.add_assign(&c_at_z);
            tmp.add_assign(&gamma);
            res.mul_assign(&tmp);

            res.sub_assign(&z_2_shifted_at_z);

            inverse_vanishing_at_z.mul_assign(&alpha);

            res.mul_assign(&inverse_vanishing_at_z);

            t_1.add_assign(&res);
        }

        {
            let mut res = z_1_shifted_at_z;
            res.sub_assign(&z_2_shifted_at_z);
            res.mul_assign(&l_n_minus_one_at_z);

            inverse_vanishing_at_z.mul_assign(&alpha);

            res.mul_assign(&inverse_vanishing_at_z);

            t_1.add_assign(&res);
        }

        {
            let mut res = z_1_at_z;
            res.sub_assign(&z_2_at_z);
            res.mul_assign(&l_0_at_z);

            inverse_vanishing_at_z.mul_assign(&alpha);

            res.mul_assign(&inverse_vanishing_at_z);

            t_1.add_assign(&res);
        }

        let mut t_at_z = E::Fr::zero();
        t_at_z.add_assign(&t_low_at_z);

        let mut tmp = z_in_pow_of_domain_size;
        tmp.mul_assign(&t_mid_at_z);
        t_at_z.add_assign(&tmp);

        let mut tmp = z_in_pow_of_domain_size;
        tmp.mul_assign(&z_in_pow_of_domain_size);
        tmp.mul_assign(&t_high_at_z);
        t_at_z.add_assign(&tmp);

        assert_eq!(t_at_z, t_1, "sanity check failed");
    }

    // we do NOT compute linearization polynomial for non-homomorphic case

    let mut z_by_omega = z;
    z_by_omega.mul_assign(&z_1.omega);

    let witness_opening_request_at_z = WitnessOpeningRequest {
        polynomials: vec![
            &a_commitment_data.poly,
            &b_commitment_data.poly,
            &c_commitment_data.poly,
            &z_1_commitment_data.poly,
            &z_2_commitment_data.poly,
            &t_poly_low_commitment_data.poly,
            &t_poly_mid_commitment_data.poly,
            &t_poly_high_commitment_data.poly
        ],
        opening_point: z,
        opening_values: vec![
            a_at_z,
            b_at_z,
            c_at_z,
            z_1_at_z,
            z_2_at_z,
            t_low_at_z,
            t_mid_at_z,
            t_high_at_z,
        ]
    };

    let witness_opening_request_at_z_omega = WitnessOpeningRequest {
        polynomials: vec![
            &z_1_commitment_data.poly,
            &z_2_commitment_data.poly,
        ],
        opening_point: z_by_omega,
        opening_values: vec![
            z_1_shifted_at_z,
            z_2_shifted_at_z,
        ]
    };

    let setup_opening_request = SetupOpeningRequest {
        polynomials: vec![
            &setup_precomp.q_l_aux.poly,
            &setup_precomp.q_r_aux.poly,
            &setup_precomp.q_o_aux.poly,
            &setup_precomp.q_m_aux.poly,
            &setup_precomp.q_c_aux.poly,
            &setup_precomp.s_id_aux.poly,
            &setup_precomp.sigma_1_aux.poly,
            &setup_precomp.sigma_2_aux.poly,
            &setup_precomp.sigma_3_aux.poly,
        ],
        setup_point: setup_precomp.q_l_aux.setup_point,
        setup_values: vec![
            setup_precomp.q_l_aux.setup_value,
            setup_precomp.q_r_aux.setup_value,
            setup_precomp.q_o_aux.setup_value,
            setup_precomp.q_m_aux.setup_value,
            setup_precomp.q_c_aux.setup_value,
            setup_precomp.s_id_aux.setup_value,
            setup_precomp.sigma_1_aux.setup_value,
            setup_precomp.sigma_2_aux.setup_value,
            setup_precomp.sigma_3_aux.setup_value,
        ],
        opening_point: z,
        opening_values: vec![
            q_l_at_z,
            q_r_at_z,
            q_o_at_z,
            q_m_at_z,
            q_c_at_z,
            s_id_at_z,
            sigma_1_at_z,
            sigma_2_at_z,
            sigma_3_at_z,
        ]
    };

    let _ = multiopening::<E, FP, T>(vec![witness_opening_request_at_z, witness_opening_request_at_z_omega], 
        vec![setup_opening_request], 
        bitreversed_omegas_for_fri, 
        &params, 
        &worker, 
        &mut transcript
    )?;

    Ok(())
}

// #[cfg(test)]
// mod test {

//     #[derive(Clone)]
//     struct BenchmarkCircuit<E: Engine>{
//         num_steps: usize,
//         _marker: std::marker::PhantomData<E>
//     }

//     impl<E: Engine> Circuit<E> for BenchmarkCircuit<E> {
//         fn synthesize<CS: ConstraintSystem<E>>(&self, cs: &mut CS) -> Result<(), SynthesisError> {
//             // yeah, fibonacci...

//             let one = E::Fr::one();
//             let mut negative_one = one;
//             negative_one.negate();

//             let mut two = one;
//             two.double();
            
//             let mut a = cs.alloc(|| {
//                 Ok(E::Fr::one())
//             })?;

//             let mut b = cs.alloc(|| {
//                 Ok(E::Fr::one())
//             })?;

//             cs.enforce_zero_2((a, b), (one, negative_one))?;
//             // cs.enforce_zero_2((b, CS::ONE), (one, negative_one))?;

//             let mut c = cs.alloc(|| {
//                 Ok(two)
//             })?;

//             cs.enforce_zero_3((a, b, c), (one, one, negative_one))?;

//             let mut a_value = one;
//             let mut b_value = one;
//             let mut c_value = two;

//             for _ in 0..self.num_steps {
//                 a = b;
//                 b = c;

//                 a_value = b_value;
//                 b_value = c_value;
//                 c_value.add_assign(&a_value);

//                 c = cs.alloc(|| {
//                     Ok(c_value)
//                 })?;

//                 cs.enforce_zero_3((a, b, c), (one, one, negative_one))?;
//             }

//             Ok(())
//         }
//     }

//     #[test]
//     fn test_bench_redshift() {
//         use crate::pairing::Engine;
//         use crate::ff::ScalarEngine;
//         use crate::plonk::transparent_engine::{TransparentEngine, PartialTwoBitReductionField};
//         use crate::plonk::transparent_engine::proth_engine::Transparent252;
//         use crate::plonk::utils::*;
//         use crate::plonk::commitments::transparent::fri::coset_combining_fri::*;
//         use crate::plonk::commitments::transparent::iop_compiler::*;
//         use crate::plonk::commitments::transcript::*;
//         use crate::plonk::commitments::*;
//         use crate::multicore::Worker;
//         // use crate::plonk::tester::*;

//         type Fr = <Transparent252 as ScalarEngine>::Fr;
//         type Transcr = Blake2sTranscript<Fr>;

//         use crate::plonk::fft::cooley_tukey_ntt::*;
//         use crate::plonk::commitments::transparent::fri::coset_combining_fri::*;
//         use crate::plonk::commitments::transparent::fri::coset_combining_fri::fri::*;
//         use crate::plonk::commitments::transparent::fri::coset_combining_fri::precomputation::*;
//         use crate::plonk::commitments::transparent::iop_compiler::*;
//         use crate::plonk::commitments::transparent::iop_compiler::coset_combining_blake2s_tree::*;

//         use std::time::Instant;

//         let log_2_rate = 4;
//         let rate = 1 << log_2_rate;
//         println!("Using rate {}", rate);
//         let sizes = vec![(1 << 18) - 10, (1 << 19) - 10, (1 << 20) - 10, (1 << 21) - 10, (1 << 22) - 10, (1 << 23) - 10];
//         let coset_schedules = vec![
//             vec![3, 3, 3, 3, 3, 3], // 18
//             vec![3, 3, 3, 3, 3, 2, 2], // 19
//             vec![3, 3, 3, 3, 3, 3, 2], // 20
//             vec![3, 3, 3, 3, 3, 3, 3], // 21 
//             vec![3, 3, 3, 3, 3, 3, 2, 2], // 22 
//             vec![3, 3, 3, 3, 3, 3, 3, 2], // 23 
//         ];

//         let worker = Worker::new();

//         for (size, coset_schedule) in sizes.into_iter().zip(coset_schedules.into_iter()) {
//             println!("Working for size {}", size);
//             let coset_params = CosetParams {
//                 cosets_schedule: coset_schedule,
//                 coset_factor: Fr::multiplicative_generator()
//             };

//             let params = RedshiftParameters {
//                 lde_factor: rate,
//                 num_queries: 4,
//                 output_coeffs_at_degree_plus_one: 1,
//                 coset_params: coset_params
//             };

//             let circuit = BenchmarkCircuit::<Transparent252> {
//                 num_steps: size,
//                 _marker: std::marker::PhantomData
//             };

//             let omegas_bitreversed = BitReversedOmegas::<Fr>::new_for_domain_size(size.next_power_of_two());
//             let omegas_inv_bitreversed = <OmegasInvBitreversed::<Fr> as CTPrecomputations::<Fr>>::new_for_domain_size(size.next_power_of_two());
//             let omegas_inv_bitreversed_for_fri = <CosetOmegasInvBitreversed::<Fr> as FriPrecomputations::<Fr>>::new_for_domain_size(size.next_power_of_two() * rate);

//             let (_, setup_precomp) = setup_with_precomputations::<Transparent252, _, _, Transcr>(
//                 &circuit,
//                 &params,
//                 &omegas_bitreversed
//             ).unwrap();

//             let mut prover = ProvingAssembly::<Transparent252>::new();
//             circuit.synthesize(&mut prover).unwrap();
//             prover.finalize();

//             println!("Start proving");

//             let start = Instant::now();

//             let _ = prover.prove_with_setup_precomputed::<_, _, _, Transcr>(
//                 &setup_precomp, 
//                 &params, 
//                 &worker, 
//                 &omegas_bitreversed, 
//                 &omegas_inv_bitreversed,
//                 &omegas_inv_bitreversed_for_fri
//             ).unwrap();

//             println!("Proving taken {:?} for size {}", start.elapsed(), size);


//         }
//     }

//     #[test]
//     fn test_ifft_using_ntt() {
//         use rand::{XorShiftRng, SeedableRng, Rand, Rng};
//         use crate::plonk::fft::cooley_tukey_ntt::*;
//         use crate::plonk::commitments::transparent::fri::coset_combining_fri::*;
//         use crate::plonk::commitments::transparent::fri::coset_combining_fri::fri::*;
//         use crate::plonk::commitments::transparent::fri::coset_combining_fri::precomputation::*;
//         use crate::pairing::Engine;
//         use crate::ff::ScalarEngine;
//         use crate::plonk::transparent_engine::{TransparentEngine, PartialTwoBitReductionField};
//         use crate::plonk::transparent_engine::proth_engine::Transparent252;

//         use crate::multicore::*;
//         use crate::source::*;

//         type Fr = <Transparent252 as ScalarEngine>::Fr;

//         let rng = &mut XorShiftRng::from_seed([0x3dbe6259, 0x8d313d76, 0x3237db17, 0xe5bc0654]);

//         let poly_sizes = vec![100, 1000, 10_000, 1_000_000];

//         let worker = Worker::new();

//         for poly_size in poly_sizes.clone().into_iter() {
//             let coeffs = (0..poly_size).map(|_| Fr::rand(rng)).collect::<Vec<_>>();
//             let poly = Polynomial::<Fr, _>::from_values(coeffs).unwrap();
//             let naive_result = poly.clone().icoset_fft_for_generator(&worker, &Fr::one());
//             let omegas_inv_bitreversed = <OmegasInvBitreversed::<Fr> as CTPrecomputations::<Fr>>::new_for_domain_size((poly_size as usize).next_power_of_two());
//             let ntt_result = poly.clone().ifft_using_bitreversed_ntt(&worker, &omegas_inv_bitreversed, &Fr::one()).unwrap();

//             assert!(naive_result == ntt_result);
//         }

//         for poly_size in poly_sizes.into_iter() {
//             let coeffs = (0..poly_size).map(|_| Fr::rand(rng)).collect::<Vec<_>>();
//             let poly = Polynomial::<Fr, _>::from_values(coeffs).unwrap();
//             let naive_result = poly.clone().icoset_fft_for_generator(&worker, &Fr::multiplicative_generator());
//             let omegas_inv_bitreversed = <OmegasInvBitreversed::<Fr> as CTPrecomputations::<Fr>>::new_for_domain_size((poly_size as usize).next_power_of_two());
//             let ntt_result = poly.clone().ifft_using_bitreversed_ntt(&worker, &omegas_inv_bitreversed, &Fr::multiplicative_generator()).unwrap();

//             assert!(naive_result == ntt_result);
//         }
//     }

//     #[test]
//     fn test_fft_test_vectors() {
//         use rand::{XorShiftRng, SeedableRng, Rand, Rng};
//         use crate::plonk::fft::*;
//         use crate::pairing::Engine;
//         use crate::ff::ScalarEngine;
//         use crate::plonk::transparent_engine::{TransparentEngine};
//         use crate::plonk::transparent_engine::proth_engine::Transparent252;

//         use crate::multicore::*;
//         use crate::source::*;

//         type Fr = <Transparent252 as ScalarEngine>::Fr;

//         let rng = &mut XorShiftRng::from_seed([0x3dbe6259, 0x8d313d76, 0x3237db17, 0xe5bc0654]);

//         let poly_sizes = vec![4, 8, 16];

//         let worker = Worker::new();

//         for poly_size in poly_sizes.clone().into_iter() {
//             println!("Poly size {}", poly_size);
//             let coeffs = (0..poly_size).map(|_| Fr::rand(rng)).collect::<Vec<_>>();
//             println!("Coefficients");
//             for c in coeffs.iter() {
//                 println!("{}", c.into_raw_repr());
//             }
//             println!("Generators");
//             let poly = Polynomial::<Fr, _>::from_coeffs(coeffs).unwrap();
//             let omega = poly.omega;
//             for i in 0..poly_size {
//                 let gen = omega.pow([i as u64]);
//                 println!("Omega^{} = {}", i, gen.into_raw_repr());
//             }
//             println!("Result");
//             let naive_result = poly.fft(&worker);
//             let coeffs = naive_result.into_coeffs();
//             for c in coeffs.iter() {
//                 println!("{}", c.into_raw_repr());
//             }
//         }
//     }
// }