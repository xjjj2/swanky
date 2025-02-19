//! This is the implementation of field conversion

use super::homcom::{FComProver, FComVerifier, MacProver, MacVerifier};
use crate::{errors::Error, svole::wykw::LpnParams};
use generic_array::typenum::Unsigned;
use rand::{CryptoRng, Rng, SeedableRng};
use scuttlebutt::{
    field::{F40b, FiniteField, F2},
    ring::FiniteRing,
    AbstractChannel, AesRng, Block, SyncChannel,
};
use std::io::{BufReader, BufWriter};
use std::net::TcpStream;
use std::time::Instant;
use subtle::{ConditionallySelectable, ConstantTimeEq};

/// EdabitsProver struct
#[derive(Clone)]
pub struct EdabitsProver<FE: FiniteField> {
    bits: Vec<MacProver<F40b>>,
    value: MacProver<FE>,
}

fn copy_edabits_prover<FE: FiniteField>(edabits: &EdabitsProver<FE>) -> EdabitsProver<FE> {
    let num_bits = edabits.bits.len();
    let mut bits_par = Vec::with_capacity(num_bits);
    for j in 0..num_bits {
        bits_par.push(edabits.bits[j].clone());
    }
    return EdabitsProver {
        bits: bits_par,
        value: edabits.value.clone(),
    };
}

/// EdabitsVerifier struct
#[derive(Clone)]
pub struct EdabitsVerifier<FE: FiniteField> {
    bits: Vec<MacVerifier<F40b>>,
    value: MacVerifier<FE>,
}

fn copy_edabits_verifier<FE: FiniteField>(edabits: &EdabitsVerifier<FE>) -> EdabitsVerifier<FE> {
    let num_bits = edabits.bits.len();
    let mut bits_par = Vec::with_capacity(num_bits);
    for j in 0..num_bits {
        bits_par.push(edabits.bits[j].clone());
    }
    return EdabitsVerifier {
        bits: bits_par,
        value: edabits.value.clone(),
    };
}

/// DabitProver struct
#[derive(Clone)]
struct DabitProver<FE: FiniteField> {
    bit: MacProver<F40b>,
    value: MacProver<FE>,
}

/// DabitVerifier struct
#[derive(Clone)]
struct DabitVerifier<FE: FiniteField> {
    bit: MacVerifier<F40b>,
    value: MacVerifier<FE>,
}

const FDABIT_SECURITY_PARAMETER: usize = 38;

/// bit to field element
fn f2_to_fe<FE: FiniteField>(b: F2) -> FE {
    let choice = b.ct_eq(&F2::ZERO);
    FE::conditional_select(&FE::ONE, &FE::ZERO, choice)
}

fn convert_bits_to_field<FE: FiniteField>(v: &[F2]) -> FE {
    let mut res = FE::ZERO;

    for b in v.iter().rev() {
        res += res; // double
        res += f2_to_fe(*b);
    }
    res
}

fn convert_bits_to_field_mac<FE: FiniteField>(v: &[MacProver<F40b>]) -> FE {
    let mut res = FE::ZERO;

    for b in v.iter().rev() {
        res += res; // double
        res += f2_to_fe(b.0);
    }
    res
}

fn power_two<FE: FiniteField>(m: usize) -> FE {
    let mut res = FE::ONE;

    for _ in 0..m {
        res += res;
    }

    res
}

// Permutation pseudorandomly generated following Fisher-Yates method
// `https://en.wikipedia.org/wiki/Fisher%E2%80%93Yates_shuffle`
fn generate_permutation<T: Clone, RNG: CryptoRng + Rng>(rng: &mut RNG, v: &mut Vec<T>) -> () {
    let size = v.len();
    if size == 0 {
        return;
    }

    let mut i = size - 1;
    while i > 0 {
        let idx = rng.gen_range(0..i);
        v.swap(idx, i);
        i -= 1;
    }
}

fn check_parameters<FE: FiniteField>(n: usize, gamma: usize) -> Result<(), Error> {
    // Because the modulus of the field might be large, we currently only store ceil(log_2(modulus))
    // for the field.
    // Let M be the modulus of the field.
    // We can use an alternate check (as follows):
    /*
    $$
    \begin{array}{ccc}
      \textsf{Invalid}& \impliedby  &  (n+1) \cdot 2^\gamma \geq \frac{M-1}{2} \\
      & \iff &  \log_2(n+1) + \gamma \geq \log_2(M-1)-1 \\
      & \impliedby &  \log_2(n+1) + \gamma \geq \lceil log_2(M) \rceil - 1 \\
      & \impliedby & \lfloor \log_2(n+1) \rfloor + \gamma \geq \lceil log_2(M) \rceil - 1
    \end{array}
    $$
    */
    // TODO: can we get away with just using the log ceiling of the modulus in this fashion?
    fn log2_floor(x: usize) -> usize {
        std::mem::size_of::<usize>() * 8
            - 1
            - usize::try_from(x.leading_zeros()).expect("sizeof(usize) >= sizeof(u32)")
    }
    if log2_floor(n + 1) + gamma >= FE::NumberOfBitsInBitDecomposition::USIZE - 1 {
        Err(Error::Other(format!(
            "Fdabit invalid parameter configuration: n={}, gamma={}, FE={}",
            n,
            gamma,
            std::any::type_name::<FE>(),
        )))
    } else {
        Ok(())
    }
}

/// Prover for the edabits conversion protocol
pub struct ProverConv<FE: FiniteField> {
    fcom_f2: FComProver<F40b>,
    fcom: FComProver<FE>,
}

// The Finite field is required to be a prime field because of the fdabit
// protocol working only for prime finite fields.
impl<FE: FiniteField<PrimeField = FE>> ProverConv<FE> {
    /// initialize the prover
    pub fn init<C: AbstractChannel, RNG: CryptoRng + Rng>(
        channel: &mut C,
        rng: &mut RNG,
        lpn_setup: LpnParams,
        lpn_extend: LpnParams,
    ) -> Result<Self, Error> {
        let a = FComProver::init(channel, rng, lpn_setup, lpn_extend)?;
        let b = FComProver::init(channel, rng, lpn_setup, lpn_extend)?;
        Ok(Self {
            fcom_f2: a,
            fcom: b,
        })
    }

    fn duplicate<C: AbstractChannel, RNG: CryptoRng + Rng>(
        &mut self,
        channel: &mut C,
        rng: &mut RNG,
    ) -> Result<Self, Error> {
        Ok(Self {
            fcom_f2: self.fcom_f2.duplicate(channel, rng)?,
            fcom: self.fcom.duplicate(channel, rng)?,
        })
    }

    fn convert_bit_2_field<C: AbstractChannel>(
        &mut self,
        channel: &mut C,
        r_batch: &[DabitProver<FE>],
        x_batch: &[MacProver<F40b>],
        c_batch: &mut Vec<MacProver<F40b>>,
        x_m_batch: &mut Vec<MacProver<FE>>,
    ) -> Result<(), Error> {
        let n = r_batch.len();
        assert_eq!(n, x_batch.len());
        c_batch.clear();
        x_m_batch.clear();

        for i in 0..n {
            c_batch.push(self.fcom_f2.add(r_batch[i].bit, x_batch[i]));
        }
        self.fcom_f2.open(channel, &c_batch)?;

        for i in 0..n {
            let MacProver(c, _) = c_batch[i];

            let c_m = f2_to_fe::<FE::PrimeField>(c);

            let choice = c.ct_eq(&F2::ONE);
            let beq = self
                .fcom
                .affine_add_cst(c_m, self.fcom.neg(r_batch[i].value));
            let bneq = self.fcom.affine_add_cst(c_m, r_batch[i].value);
            let x_m = MacProver::conditional_select(&bneq, &beq, choice);

            x_m_batch.push(x_m);
        }

        assert_eq!(n, x_m_batch.len());
        Ok(())
    }

    // This function applies the bit_add_carry to a batch of bits,
    // contrary to the one in the paper that applies it on a pair of
    // bits. This allows to the keep the rounds of communication equal
    // to m for any vector of additions
    fn bit_add_carry<C: AbstractChannel, RNG: CryptoRng + Rng>(
        &mut self,
        channel: &mut C,
        rng: &mut RNG,
        x_batch: &[EdabitsProver<FE>],
        y_batch: &[EdabitsProver<FE>],
        random_triples: &[(MacProver<F40b>, MacProver<F40b>, MacProver<F40b>)],
    ) -> Result<Vec<(Vec<MacProver<F40b>>, MacProver<F40b>)>, Error> {
        let num = x_batch.len();
        if num != y_batch.len() {
            return Err(Error::Other(
                "incompatible input vectors in bit_add_carry".to_string(),
            ));
        }

        let m = x_batch[0].bits.len();

        // input c0
        let mut ci_batch = vec![F2::ZERO; num];
        let mut ci_mac_batch = self.fcom_f2.input(channel, rng, &ci_batch)?;

        // loop on the m bits over the batch of n addition
        let mut triples = Vec::with_capacity(num * m);
        let mut aux_batch = Vec::with_capacity(num);
        let mut and_res_batch = Vec::with_capacity(num);
        let mut z_batch = vec![Vec::with_capacity(m); num];
        let mut and_res_mac_batch = Vec::with_capacity(num);
        for i in 0..m {
            and_res_batch.clear();
            aux_batch.clear();
            for n in 0..num {
                let ci_clr = ci_batch[n];
                let ci_mac = ci_mac_batch[n];

                let ci = MacProver(ci_clr, ci_mac);

                let x = &x_batch[n].bits;
                let y = &y_batch[n].bits;

                debug_assert_eq!(x.len(), m);
                debug_assert_eq!(y.len(), m);

                let xi = x[i];
                let yi = y[i];

                let and1 = self.fcom_f2.add(xi, ci);
                let MacProver(and1_clr, _) = and1;
                let and2 = self.fcom_f2.add(yi, ci);

                let and_res = and1_clr * and2.0;

                let c = ci_clr + and_res;
                // let c_mac = ci_mac + and_res_mac; // is done in the next step
                ci_batch[n] = c;

                let z = self.fcom_f2.add(and1, yi); // xi + yi + ci ;
                z_batch[n].push(z);

                and_res_batch.push(and_res);
                aux_batch.push((and1, and2));
            }
            and_res_mac_batch.clear();
            self.fcom_f2
                .input_low_level(channel, rng, &and_res_batch, &mut and_res_mac_batch)?;

            for n in 0..num {
                let (and1, and2) = aux_batch[n];
                let and_res = and_res_batch[n];
                let and_res_mac = and_res_mac_batch[n];
                triples.push((and1, and2, MacProver(and_res, and_res_mac)));

                let ci_mac = ci_mac_batch[n];
                let c_mac = ci_mac + and_res_mac;

                ci_mac_batch[n] = c_mac;
            }
        }

        // check all the multiplications in one batch
        channel.flush()?;
        if random_triples.len() == 0 {
            self.fcom_f2
                .quicksilver_check_multiply(channel, rng, &triples)?;
        } else {
            self.fcom_f2
                .wolverine_check_multiply(channel, &triples, &random_triples)?;
        }

        // reconstruct the solution
        let mut res = Vec::with_capacity(num);

        let mut i = 0;
        for zs in z_batch.into_iter() {
            res.push((zs, MacProver(ci_batch[i], ci_mac_batch[i])));
            i += 1;
        }

        Ok(res)
    }

    /// generate random edabits
    pub fn random_edabits<C: AbstractChannel, RNG: CryptoRng + Rng>(
        &mut self,
        channel: &mut C,
        rng: &mut RNG,
        nb_bits: usize,
        num: usize, // in the paper: NB + C
    ) -> Result<Vec<EdabitsProver<FE>>, Error> {
        let mut edabits_vec = Vec::with_capacity(num);

        let mut aux_bits = Vec::with_capacity(num);
        let mut aux_r_m = Vec::with_capacity(num);
        for _ in 0..num {
            let mut bits = Vec::with_capacity(nb_bits);
            for _ in 0..nb_bits {
                bits.push(self.fcom_f2.random(channel, rng)?);
            }
            let r_m: FE::PrimeField = convert_bits_to_field::<FE::PrimeField>(
                bits.iter().map(|x| x.0).collect::<Vec<F2>>().as_slice(),
            );
            aux_bits.push(bits);
            aux_r_m.push(r_m);
        }

        let aux_r_m_mac: Vec<FE> = self.fcom.input(channel, rng, &aux_r_m)?;

        let mut i = 0;
        for aux_bits in aux_bits.into_iter() {
            edabits_vec.push(EdabitsProver {
                bits: aux_bits,
                value: MacProver(aux_r_m[i], aux_r_m_mac[i]),
            });
            i += 1;
        }
        Ok(edabits_vec)
    }

    fn random_dabits<C: AbstractChannel, RNG: CryptoRng + Rng>(
        &mut self,
        channel: &mut C,
        rng: &mut RNG,
        num: usize,
    ) -> Result<Vec<DabitProver<FE>>, Error> {
        let mut dabit_vec = Vec::with_capacity(num);
        let mut b_batch = Vec::with_capacity(num);
        let mut b_m_batch = Vec::with_capacity(num);

        for _ in 0..num {
            let b = self.fcom_f2.random(channel, rng)?;
            b_batch.push(b);
            let b_m = f2_to_fe(b.0);
            b_m_batch.push(b_m);
        }

        let b_m_mac_batch = self.fcom.input(channel, rng, &b_m_batch)?;

        for i in 0..num {
            dabit_vec.push(DabitProver {
                bit: b_batch[i],
                value: MacProver(b_m_batch[i], b_m_mac_batch[i]),
            });
        }
        Ok(dabit_vec)
    }

    /// Generate random triples
    pub fn random_triples<C: AbstractChannel, RNG: CryptoRng + Rng>(
        &mut self,
        channel: &mut C,
        rng: &mut RNG,
        num: usize,
        out: &mut Vec<(MacProver<F40b>, MacProver<F40b>, MacProver<F40b>)>,
    ) -> Result<(), Error> {
        let mut pairs = Vec::with_capacity(num);
        let mut zs = Vec::with_capacity(num);
        for _ in 0..num {
            let x = self.fcom_f2.random(channel, rng)?;
            let y = self.fcom_f2.random(channel, rng)?;
            let z = x.0 * y.0;
            pairs.push((x, y));
            zs.push(z);
        }
        let mut zs_mac = Vec::with_capacity(num);
        self.fcom_f2
            .input_low_level(channel, rng, &zs, &mut zs_mac)?;

        for i in 0..num {
            let (x, y) = pairs[i];
            let z = zs[i];
            let z_mac = zs_mac[i];
            out.push((x, y, MacProver(z, z_mac)));
        }
        channel.flush()?;
        Ok(())
    }

    fn fdabit<C: AbstractChannel, RNG: CryptoRng + Rng>(
        &mut self,
        channel: &mut C,
        rng: &mut RNG,
        dabits: &Vec<DabitProver<FE>>,
    ) -> Result<(), Error> {
        let s = FDABIT_SECURITY_PARAMETER;
        let n = dabits.len();

        let num_bits = std::mem::size_of::<usize>() * 8;
        let gamma = num_bits - ((n + 1).leading_zeros() as usize) - 1 + 1;

        check_parameters::<FE>(n, gamma)?;

        let mut res = true;

        for i in 0..n {
            // making sure the faulty dabits are not faulty
            debug_assert!(
                ((dabits[i].bit.0 == F2::ZERO) & (dabits[i].value.0 == FE::PrimeField::ZERO))
                    | ((dabits[i].bit.0 == F2::ONE) & (dabits[i].value.0 == FE::PrimeField::ONE))
            );
        }

        // step 1)
        let mut c_m: Vec<Vec<FE::PrimeField>> = vec![Vec::with_capacity(gamma); s];
        let mut c_m_mac: Vec<Vec<FE>> = Vec::with_capacity(s);
        for k in 0..s {
            for _ in 0..gamma {
                let b: F2 = F2::random(rng);
                let b_m = f2_to_fe(b);
                c_m[k].push(b_m);
            }
        }

        for k in 0..s {
            let b_m_mac = self.fcom.input(channel, rng, c_m[k].as_slice())?;
            c_m_mac.push(b_m_mac);
        }

        let mut c1: Vec<F2> = Vec::with_capacity(s);
        for k in 0..s {
            if c_m[k][0] == FE::PrimeField::ZERO {
                c1.push(F2::ZERO);
            } else {
                c1.push(F2::ONE);
            }
        }
        let c1_mac = self.fcom_f2.input(channel, rng, &c1)?;

        // step 2)
        let mut triples = Vec::with_capacity(gamma * s);
        let mut andl_batch = Vec::with_capacity(gamma * s);
        let mut andl_mac_batch = Vec::with_capacity(gamma * s);
        let mut one_minus_ci_batch = Vec::with_capacity(gamma * s);
        let mut one_minus_ci_mac_batch = Vec::with_capacity(gamma * s);
        let mut and_res_batch = Vec::with_capacity(gamma * s);
        for k in 0..s {
            for i in 0..gamma {
                let andl: FE::PrimeField = c_m[k][i];
                let andl_mac: FE = c_m_mac[k][i];
                let MacProver(minus_ci, minus_ci_mac) = // -ci
                    self.fcom.affine_mult_cst(-FE::PrimeField::ONE, MacProver(andl, andl_mac));
                let MacProver(one_minus_ci, one_minus_ci_mac) = // 1 - ci
                    self.fcom.affine_add_cst(FE::PrimeField::ONE, MacProver(minus_ci, minus_ci_mac));
                let and_res = andl * one_minus_ci;
                andl_batch.push(andl);
                andl_mac_batch.push(andl_mac);
                one_minus_ci_batch.push(one_minus_ci);
                one_minus_ci_mac_batch.push(one_minus_ci_mac);
                and_res_batch.push(and_res);
            }
        }
        let and_res_mac_batch = self.fcom.input(channel, rng, &and_res_batch)?;

        for j in 0..s * gamma {
            triples.push((
                MacProver(andl_batch[j], andl_mac_batch[j]),
                MacProver(one_minus_ci_batch[j], one_minus_ci_mac_batch[j]),
                MacProver(and_res_batch[j], and_res_mac_batch[j]),
            ));
        }

        // step 3)
        channel.flush()?;
        let seed = channel.read_block()?;
        let mut e_rng = AesRng::from_seed(seed);
        let mut e = vec![Vec::with_capacity(n); s];
        for k in 0..s {
            for _i in 0..n {
                let b = F2::random(&mut e_rng);
                e[k].push(b);
            }
        }

        // step 4)
        let mut r_batch = Vec::with_capacity(s);
        for k in 0..s {
            let (mut r, mut r_mac) = (c1[k], c1_mac[k]);
            for i in 0..n {
                // TODO: do not need to do it when e[i] is ZERO
                let MacProver(tmp, tmp_mac) = self.fcom_f2.affine_mult_cst(e[k][i], dabits[i].bit);
                debug_assert!(
                    ((e[k][i] == F2::ONE) & (tmp == dabits[i].bit.0)) | (tmp == F2::ZERO)
                );
                r += tmp;
                r_mac += tmp_mac;
            }
            r_batch.push(MacProver(r, r_mac));
        }

        // step 5) TODO: move this to the end
        let _ = self.fcom_f2.open(channel, &r_batch)?;

        // step 6)
        let mut r_prime_batch = Vec::with_capacity(s);
        for k in 0..s {
            // step 6)
            // NOTE: for performance maybe step 4 and 6 should be combined in one loop
            let (mut r_prime, mut r_prime_mac) = (FE::PrimeField::ZERO, FE::ZERO);
            for i in 0..n {
                // TODO: do not need to do it when e[i] is ZERO
                let b = f2_to_fe(e[k][i]);
                let MacProver(tmp, tmp_mac) = self.fcom.affine_mult_cst(b, dabits[i].value);
                debug_assert!(
                    ((b == FE::PrimeField::ONE) & (tmp == dabits[i].value.0))
                        | (tmp == FE::PrimeField::ZERO)
                );
                r_prime += tmp;
                r_prime_mac += tmp_mac;
            }
            r_prime_batch.push((r_prime, r_prime_mac));
        }

        // step 7)
        let mut tau_batch = Vec::with_capacity(s);
        for k in 0..s {
            let (mut tau, mut tau_mac) = r_prime_batch[k];
            let mut twos = FE::PrimeField::ONE;
            for i in 0..gamma {
                let MacProver(tmp, tmp_mac) = self
                    .fcom
                    .affine_mult_cst(twos, MacProver(c_m[k][i], c_m_mac[k][i]));
                if i == 0 {
                    debug_assert!(c_m[k][i] == tmp);
                }
                tau += tmp;
                tau_mac += tmp_mac;
                twos += twos;
            }
            tau_batch.push(MacProver(tau, tau_mac));
        }

        let _ = self.fcom.open(channel, &tau_batch)?;

        // step 8)
        for k in 0..s {
            // step 8)
            // NOTE: This is not needed for the prover,
            let b =
                // mod2 is computed using the first bit of the bit decomposition.
                // NOTE: This scales linearly with the size of the bit decomposition and could lead to potential inefficiencies
                (r_batch[k].0 == F2::ONE) == tau_batch[k].0.bit_decomposition()[0];
            res = res & b;
        }
        self.fcom
            .quicksilver_check_multiply(channel, rng, &triples)?;

        if res {
            Ok(())
        } else {
            Err(Error::Other("fail fdabit prover".to_string()))
        }
    }

    fn conv_loop<C: AbstractChannel, RNG: CryptoRng + Rng>(
        &mut self,
        channel: &mut C,
        rng: &mut RNG,
        edabits_vector: &[EdabitsProver<FE>],
        r: &[EdabitsProver<FE>],
        dabits: &[DabitProver<FE>],
        convert_bit_2_field_aux: &mut Vec<MacProver<F40b>>,
        e_m_batch: &mut Vec<MacProver<FE>>,
        random_triples: &[(MacProver<F40b>, MacProver<F40b>, MacProver<F40b>)],
    ) -> Result<(), Error> {
        let n = edabits_vector.len();
        let nb_bits = edabits_vector[0].bits.len();
        let power_two_nb_bits = power_two::<FE::PrimeField>(nb_bits);
        // step 6)b) batched and moved up
        let e_batch = self.bit_add_carry(channel, rng, &edabits_vector, &r, &random_triples)?;

        // step 6)c) batched and moved up
        let mut e_carry_batch = Vec::with_capacity(n);
        for (_, e_carry) in e_batch.iter() {
            e_carry_batch.push(e_carry.clone());
        }

        self.convert_bit_2_field(
            channel,
            &dabits,
            &e_carry_batch,
            convert_bit_2_field_aux,
            e_m_batch,
        )?;

        // 6)a)
        let mut e_prime_batch = Vec::with_capacity(n);
        // 6)d)
        let mut ei_batch = Vec::with_capacity(n * nb_bits);
        for i in 0..n {
            // 6)a)
            let c_m = edabits_vector[i].value;
            let r_m = r[i].value;
            let c_plus_r = self.fcom.add(c_m, r_m);

            // 6)c) done earlier
            let e_m = e_m_batch[i];

            // 6)d)
            let e_prime = self
                .fcom
                .add(c_plus_r, self.fcom.affine_mult_cst(-power_two_nb_bits, e_m));
            e_prime_batch.push(e_prime);
            ei_batch.extend(&e_batch[i].0);
        }

        // 6)e)
        self.fcom_f2.open(channel, &ei_batch)?;

        let mut e_prime_minus_sum_batch = Vec::with_capacity(n);
        for i in 0..n {
            let sum = convert_bits_to_field_mac::<FE>(&ei_batch[i * nb_bits..(i + 1) * nb_bits]);
            e_prime_minus_sum_batch.push(self.fcom.affine_add_cst(-sum, e_prime_batch[i]));
        }

        // Remark this is not necessary for the prover, bc cst addition dont show up in mac
        // let s = convert_f2_to_field(ei);
        self.fcom.check_zero(channel, &e_prime_minus_sum_batch)?;
        Ok(())
    }

    /// conversion checking
    pub fn conv<C: AbstractChannel, RNG: CryptoRng + Rng>(
        &mut self,
        channel: &mut C,
        rng: &mut RNG,
        num_bucket: usize,
        num_cut: usize,
        edabits_vector: &[EdabitsProver<FE>],
        bucket_channels: Option<Vec<SyncChannel<BufReader<TcpStream>, BufWriter<TcpStream>>>>,
        with_quicksilver: bool,
    ) -> Result<(), Error> {
        let n = edabits_vector.len();
        let nb_bits = edabits_vector[0].bits.len();

        let nb_random_edabits = n * num_bucket + num_cut;
        let nb_random_dabits = n * num_bucket;

        // step 1)a): commit random edabit
        let mut r = self.random_edabits(channel, rng, nb_bits, nb_random_edabits)?;

        // step 1)b)
        let mut dabits = self.random_dabits(channel, rng, nb_random_dabits)?;

        // step 1)c): multiplication triples
        let mut random_triples = Vec::new();
        if !with_quicksilver {
            // with wolverine
            let how_many = num_bucket * n * nb_bits + num_cut * nb_bits;
            self.random_triples(channel, rng, how_many, &mut random_triples)?;
        }

        // step 2)
        self.fdabit(channel, rng, &dabits)?;

        // step 3) get seed for permutation
        let seed = channel.read_block()?;
        let mut shuffle_rng = AesRng::from_seed(seed);

        // step 4): shuffle edabits, dabits and triples
        generate_permutation(&mut shuffle_rng, &mut r);
        generate_permutation(&mut shuffle_rng, &mut dabits);
        generate_permutation(&mut shuffle_rng, &mut random_triples);

        // step 5)a):
        let base = n * num_bucket;
        for i in 0..num_cut {
            let idx = base + i;
            let a = &r[idx];
            self.fcom_f2.open(channel, &a.bits)?;
            self.fcom.open(channel, &[a.value])?;
        }

        // step 5) b):
        if !with_quicksilver {
            let base = n * num_bucket * nb_bits;
            for i in 0..num_cut * nb_bits {
                let (x, y, z) = random_triples[base + i];
                let _res = self.fcom_f2.open(channel, &[x, y])?;
                let v = self.fcom_f2.affine_add_cst(-(x.0 * y.0), z);
                self.fcom_f2.check_zero(channel, &[v])?;
            }
        }

        // step 6)
        if bucket_channels.is_none() {
            let mut convert_bit_2_field_aux = Vec::with_capacity(n);
            let mut e_m_batch = Vec::with_capacity(n);
            for j in 0..num_bucket {
                // base index for the window of `idx_base..idx_base + n` values
                let idx_base = j * n;

                if with_quicksilver {
                    self.conv_loop(
                        channel,
                        rng,
                        &edabits_vector,
                        &r[idx_base..idx_base + n],
                        &dabits[idx_base..idx_base + n],
                        &mut convert_bit_2_field_aux,
                        &mut e_m_batch,
                        &Vec::new(),
                    )?;
                } else {
                    self.conv_loop(
                        channel,
                        rng,
                        &edabits_vector,
                        &r[idx_base..idx_base + n],
                        &dabits[idx_base..idx_base + n],
                        &mut convert_bit_2_field_aux,
                        &mut e_m_batch,
                        &random_triples[idx_base * nb_bits..idx_base * nb_bits + n * nb_bits],
                    )?;
                }
            }
        } else {
            let mut j = 0;
            let mut handles = Vec::new();
            for mut bucket_channel in bucket_channels.unwrap().into_iter() {
                // splitting the vectors to spawn
                let idx_base = j * n;
                let mut edabits_vector_par = Vec::with_capacity(n);
                for edabits in edabits_vector.iter() {
                    edabits_vector_par.push(copy_edabits_prover(edabits));
                }

                let mut r_par = Vec::with_capacity(n);
                for r_elm in r[idx_base..idx_base + n].iter() {
                    r_par.push(copy_edabits_prover(r_elm));
                }

                let mut dabits_par = Vec::with_capacity(n);
                for elm in dabits[idx_base..idx_base + n].iter() {
                    dabits_par.push(elm.clone());
                }

                let mut random_triples_par = Vec::new(); //with_capacity(n * nb_bits);
                if !with_quicksilver {
                    //let mut random_triples_par = Vec::with_capacity(n * nb_bits);
                    for elm in
                        random_triples[idx_base * nb_bits..idx_base * nb_bits + n * nb_bits].iter()
                    {
                        random_triples_par.push(elm.clone());
                    }
                }

                let mut new_prover = self.duplicate(channel, rng)?;
                let handle = std::thread::spawn(move || {
                    let mut convert_bit_2_field_aux = Vec::with_capacity(n);
                    let mut e_m_batch = Vec::with_capacity(n);
                    new_prover.conv_loop(
                        &mut bucket_channel,
                        &mut AesRng::new(),
                        &edabits_vector_par,
                        &r_par,
                        &dabits_par,
                        &mut convert_bit_2_field_aux,
                        &mut e_m_batch,
                        &random_triples_par,
                    )
                });
                handles.push(handle);

                j += 1;
            }

            for handle in handles {
                handle.join().unwrap().unwrap();
            }
        }

        Ok(())
    }
}

/// Verifier for the edabits conversion protocol
pub struct VerifierConv<FE: FiniteField> {
    fcom_f2: FComVerifier<F40b>,
    fcom: FComVerifier<FE>,
}

// The Finite field is required to be a prime field because of the fdabit
// protocol working only for prime finite fields.
impl<FE: FiniteField<PrimeField = FE>> VerifierConv<FE> {
    /// initialize the verifier
    pub fn init<C: AbstractChannel, RNG: CryptoRng + Rng>(
        channel: &mut C,
        rng: &mut RNG,
        lpn_setup: LpnParams,
        lpn_extend: LpnParams,
    ) -> Result<Self, Error> {
        let a = FComVerifier::init(channel, rng, lpn_setup, lpn_extend)?;
        let b = FComVerifier::init(channel, rng, lpn_setup, lpn_extend)?;
        Ok(Self {
            fcom_f2: a,
            fcom: b,
        })
    }

    fn duplicate<C: AbstractChannel, RNG: CryptoRng + Rng>(
        &mut self,
        channel: &mut C,
        rng: &mut RNG,
    ) -> Result<Self, Error> {
        Ok(Self {
            fcom_f2: self.fcom_f2.duplicate(channel, rng)?,
            fcom: self.fcom.duplicate(channel, rng)?,
        })
    }

    fn convert_bit_2_field<C: AbstractChannel>(
        &mut self,
        channel: &mut C,
        r_batch: &[DabitVerifier<FE>],
        x_batch: &[MacVerifier<F40b>],
        r_mac_plus_x_mac: &mut Vec<MacVerifier<F40b>>,
        c_batch: &mut Vec<F2>,
        x_m_batch: &mut Vec<MacVerifier<FE>>,
    ) -> Result<(), Error> {
        let n = r_batch.len();
        debug_assert!(n == x_batch.len());
        r_mac_plus_x_mac.clear();
        x_m_batch.clear();

        for i in 0..n {
            r_mac_plus_x_mac.push(self.fcom_f2.add(r_batch[i].bit, x_batch[i]));
        }
        self.fcom_f2.open(channel, &r_mac_plus_x_mac, c_batch)?;

        for i in 0..n {
            let c = c_batch[i];

            let c_m = f2_to_fe::<FE::PrimeField>(c);

            let choice = c.ct_eq(&F2::ONE);
            let beq = self
                .fcom
                .affine_add_cst(c_m, self.fcom.neg(r_batch[i].value));
            let bneq = self.fcom.affine_add_cst(c_m, r_batch[i].value);
            let x_m = MacVerifier::conditional_select(&bneq, &beq, choice);

            x_m_batch.push(x_m);
        }

        assert_eq!(n, x_m_batch.len());
        Ok(())
    }

    fn bit_add_carry<C: AbstractChannel, RNG: CryptoRng + Rng>(
        &mut self,
        channel: &mut C,
        rng: &mut RNG,
        x_batch: &[EdabitsVerifier<FE>],
        y_batch: &[EdabitsVerifier<FE>],
        random_triples: &[(MacVerifier<F40b>, MacVerifier<F40b>, MacVerifier<F40b>)],
    ) -> Result<Vec<(Vec<MacVerifier<F40b>>, MacVerifier<F40b>)>, Error> {
        let num = x_batch.len();
        if num != y_batch.len() {
            return Err(Error::Other(
                "incompatible input vectors in bit_add_carry".to_string(),
            ));
        }

        let m = x_batch[0].bits.len();

        // input c0
        let mut ci_batch = self.fcom_f2.input(channel, rng, num)?;

        // loop on the m bits over the batch of n addition
        let mut triples = Vec::with_capacity(num * m);
        let mut aux_batch = Vec::with_capacity(num);
        let mut z_batch = vec![Vec::with_capacity(m); num];
        let mut and_res_mac_batch = Vec::with_capacity(num);
        for i in 0..m {
            aux_batch.clear();
            for n in 0..num {
                let ci = ci_batch[n];

                let x = &x_batch[n].bits;
                let y = &y_batch[n].bits;

                debug_assert!(x.len() == m && y.len() == m);

                let xi = x[i];
                let yi = y[i];

                let and1 = self.fcom_f2.add(xi, ci);
                let and2 = self.fcom_f2.add(yi, ci);

                let z = self.fcom_f2.add(and1, yi); //xi_mac + yi_mac + ci_mac;
                z_batch[n].push(z);
                aux_batch.push((and1, and2));
            }
            and_res_mac_batch.clear();
            self.fcom_f2
                .input_low_level(channel, rng, num, &mut and_res_mac_batch)?;

            for n in 0..num {
                let (and1_mac, and2_mac) = aux_batch[n];
                let and_res_mac = and_res_mac_batch[n];
                triples.push((and1_mac, and2_mac, and_res_mac));

                let ci = ci_batch[n];
                let c_mac = self.fcom_f2.add(ci, and_res_mac);
                ci_batch[n] = c_mac;
            }
        }
        // check all the multiplications in one batch
        if random_triples.len() == 0 {
            self.fcom_f2
                .quicksilver_check_multiply(channel, rng, &triples)?;
        } else {
            self.fcom_f2
                .wolverine_check_multiply(channel, rng, &triples, &random_triples)?;
        }
        // reconstruct the solution
        let mut res = Vec::with_capacity(num);
        let mut i = 0;
        for zs in z_batch.into_iter() {
            res.push((zs, ci_batch[i]));
            i += 1;
        }

        Ok(res)
    }

    /// generate random edabits
    pub fn random_edabits<C: AbstractChannel, RNG: CryptoRng + Rng>(
        &mut self,
        channel: &mut C,
        rng: &mut RNG,
        nb_bits: usize,
        num: usize, // in the paper: NB + C
    ) -> Result<Vec<EdabitsVerifier<FE>>, Error> {
        let mut edabits_vec_mac = Vec::with_capacity(num);
        let mut aux_bits = Vec::with_capacity(num);
        for _ in 0..num {
            let mut bits = Vec::with_capacity(nb_bits);
            for _ in 0..nb_bits {
                bits.push(self.fcom_f2.random(channel, rng)?);
            }
            aux_bits.push(bits);
        }

        let aux_r_m_mac = self.fcom.input(channel, rng, num)?;

        let mut i = 0;
        for aux_bits in aux_bits.into_iter() {
            edabits_vec_mac.push(EdabitsVerifier {
                bits: aux_bits,
                value: aux_r_m_mac[i],
            });
            i += 1;
        }
        Ok(edabits_vec_mac)
    }

    fn random_dabits<C: AbstractChannel, RNG: CryptoRng + Rng>(
        &mut self,
        channel: &mut C,
        rng: &mut RNG,
        num: usize,
    ) -> Result<Vec<DabitVerifier<FE>>, Error> {
        let mut dabit_vec_mac = Vec::with_capacity(num);
        let mut b_mac_batch = Vec::with_capacity(num);
        for _ in 0..num {
            b_mac_batch.push(self.fcom_f2.random(channel, rng)?);
        }
        let b_m_mac_batch = self.fcom.input(channel, rng, num)?;
        for i in 0..num {
            dabit_vec_mac.push(DabitVerifier {
                bit: b_mac_batch[i],
                value: b_m_mac_batch[i],
            });
        }
        Ok(dabit_vec_mac)
    }

    /// Generate random triples
    pub fn random_triples<C: AbstractChannel, RNG: CryptoRng + Rng>(
        &mut self,
        channel: &mut C,
        rng: &mut RNG,
        num: usize,
        out: &mut Vec<(MacVerifier<F40b>, MacVerifier<F40b>, MacVerifier<F40b>)>,
    ) -> Result<(), Error> {
        let mut pairs = Vec::with_capacity(num);
        for _ in 0..num {
            let x = self.fcom_f2.random(channel, rng)?;
            let y = self.fcom_f2.random(channel, rng)?;
            pairs.push((x, y));
        }
        let mut zs = Vec::with_capacity(num);
        self.fcom_f2.input_low_level(channel, rng, num, &mut zs)?;

        for i in 0..num {
            let (x, y) = pairs[i];
            let z = zs[i];
            out.push((x, y, z));
        }
        Ok(())
    }

    fn fdabit<C: AbstractChannel, RNG: CryptoRng + Rng>(
        &mut self,
        channel: &mut C,
        rng: &mut RNG,
        dabits_mac: &Vec<DabitVerifier<FE>>,
    ) -> Result<(), Error> {
        let s = FDABIT_SECURITY_PARAMETER;
        let n = dabits_mac.len();

        let num_bits = std::mem::size_of::<usize>() * 8;
        let gamma = num_bits - ((n + 1).leading_zeros() as usize) - 1 + 1;

        check_parameters::<FE>(n, gamma)?;

        let mut res = true;

        // step 1)
        let mut c_m_mac: Vec<Vec<MacVerifier<FE>>> = Vec::with_capacity(s);
        for _ in 0..s {
            let b_m_mac = self.fcom.input(channel, rng, gamma)?;
            c_m_mac.push(b_m_mac);
        }

        let c1_mac = self.fcom_f2.input(channel, rng, s)?;

        // step 2)
        let mut triples = Vec::with_capacity(gamma * s);
        let mut andl_mac_batch = Vec::with_capacity(gamma * s);
        let mut one_minus_ci_mac_batch = Vec::with_capacity(gamma * s);
        for k in 0..s {
            for i in 0..gamma {
                let andl_mac = c_m_mac[k][i];
                let minus_ci_mac = // -ci
                    self.fcom.affine_mult_cst(-FE::PrimeField::ONE, andl_mac);
                let one_minus_ci_mac = // 1 - ci
                    self.fcom.affine_add_cst(FE::PrimeField::ONE, minus_ci_mac);
                andl_mac_batch.push(andl_mac);
                one_minus_ci_mac_batch.push(one_minus_ci_mac);
            }
        }

        let and_res_mac_batch = self.fcom.input(channel, rng, gamma * s)?;
        for j in 0..s * gamma {
            triples.push((
                andl_mac_batch[j],
                one_minus_ci_mac_batch[j],
                and_res_mac_batch[j],
            ));
        }

        // step 3)
        let seed = rng.gen::<Block>();
        channel.write_block(&seed)?;
        channel.flush()?;
        let mut e_rng = AesRng::from_seed(seed);
        let mut e = vec![Vec::with_capacity(n); s];
        for k in 0..s {
            for _i in 0..n {
                let b = F2::random(&mut e_rng);
                e[k].push(b);
            }
        }

        // step 4)
        let mut r_mac_batch = Vec::with_capacity(s);
        for k in 0..s {
            let mut r_mac = c1_mac[k].0;
            for i in 0..n {
                // TODO: do not need to do it when e[i] is ZERO
                let MacVerifier(tmp_mac) = self.fcom_f2.affine_mult_cst(e[k][i], dabits_mac[i].bit);
                r_mac += tmp_mac;
            }
            r_mac_batch.push(MacVerifier(r_mac));
        }

        // step 5)
        let mut r_batch = Vec::with_capacity(s);
        self.fcom_f2.open(channel, &r_mac_batch, &mut r_batch)?;

        // step 6)
        let mut r_prime_batch = Vec::with_capacity(s);
        for k in 0..s {
            // NOTE: for performance maybe step 4 and 6 should be combined in one loop
            let mut r_prime_mac = FE::ZERO;
            for i in 0..n {
                // TODO: do not need to do it when e[i] is ZERO
                let b = f2_to_fe(e[k][i]);
                let MacVerifier(tmp_mac) = self.fcom.affine_mult_cst(b, dabits_mac[i].value);
                r_prime_mac += tmp_mac;
            }
            r_prime_batch.push(r_prime_mac);
        }

        // step 7)
        let mut tau_mac_batch = Vec::with_capacity(s);
        for k in 0..s {
            let mut tau_mac = r_prime_batch[k];
            let mut twos = FE::PrimeField::ONE;
            for i in 0..gamma {
                let MacVerifier(tmp_mac) = self.fcom.affine_mult_cst(twos, c_m_mac[k][i]);
                tau_mac += tmp_mac;
                twos += twos;
            }
            tau_mac_batch.push(MacVerifier(tau_mac));
        }

        let mut tau_batch = Vec::with_capacity(s);
        self.fcom.open(channel, &tau_mac_batch, &mut tau_batch)?;

        // step 8)
        for k in 0..s {
            let b =
                // mod2 is computed using the first bit of the bit decomposition.
                // NOTE: This scales linearly with the size of the bit decomposition and could lead to potential inefficiencies
                (r_batch[k] == F2::ONE) == tau_batch[k].bit_decomposition()[0];
            res = res & b;
        }
        self.fcom
            .quicksilver_check_multiply(channel, rng, &triples)?;

        if res {
            Ok(())
        } else {
            Err(Error::Other("fail fdabit verifier".to_string()))
        }
    }

    fn conv_loop<C: AbstractChannel, RNG: CryptoRng + Rng>(
        &mut self,
        channel: &mut C,
        rng: &mut RNG,
        edabits_vector_mac: &[EdabitsVerifier<FE>],
        r_mac: &[EdabitsVerifier<FE>],
        dabits_mac: &[DabitVerifier<FE>],
        convert_bit_2_field_aux1: &mut Vec<MacVerifier<F40b>>,
        convert_bit_2_field_aux2: &mut Vec<F2>,
        e_m_batch: &mut Vec<MacVerifier<FE>>,
        ei_batch: &mut Vec<F2>,
        random_triples: &[(MacVerifier<F40b>, MacVerifier<F40b>, MacVerifier<F40b>)],
    ) -> Result<(), Error> {
        let n = edabits_vector_mac.len();
        let nb_bits = edabits_vector_mac[0].bits.len();
        let power_two_nb_bits = power_two::<FE::PrimeField>(nb_bits);

        // step 6)b) batched and moved up
        print!("ADD< ... ");
        let start = Instant::now();
        let e_batch =
            self.bit_add_carry(channel, rng, edabits_vector_mac, &r_mac, &random_triples)?;
        println!("ADD> {:?}", start.elapsed());

        // step 6)c) batched and moved up
        print!("A2B< ...");
        let start = Instant::now();
        let mut e_carry_mac_batch = Vec::with_capacity(n);
        for (_, e_carry) in e_batch.iter() {
            e_carry_mac_batch.push(e_carry.clone());
        }

        self.convert_bit_2_field(
            channel,
            &dabits_mac,
            &e_carry_mac_batch,
            convert_bit_2_field_aux1,
            convert_bit_2_field_aux2,
            e_m_batch,
        )?;
        println!("A2B> {:?}", start.elapsed());

        // 6)a)
        let mut e_prime_mac_batch = Vec::with_capacity(n);
        // 6)d)
        let mut ei_mac_batch = Vec::with_capacity(n * nb_bits);
        for i in 0..n {
            // 6)a)
            let c_m = edabits_vector_mac[i].value;
            let r_m = r_mac[i].value;
            let c_plus_r = self.fcom.add(c_m, r_m);

            // 6)c) done earlier
            let e_m = e_m_batch[i];

            // 6)d)
            let e_prime = self
                .fcom
                .add(c_plus_r, self.fcom.affine_mult_cst(-power_two_nb_bits, e_m));
            e_prime_mac_batch.push(e_prime);

            // 6)e)
            ei_mac_batch.extend(&e_batch[i].0);
        }
        // 6)e)
        print!("OPEN< ... ");
        let start = Instant::now();
        self.fcom_f2.open(channel, &ei_mac_batch, ei_batch)?;
        println!("OPEN> {:?}", start.elapsed());

        let mut e_prime_minus_sum_batch = Vec::with_capacity(n);
        for i in 0..n {
            let sum =
                convert_bits_to_field::<FE::PrimeField>(&ei_batch[i * nb_bits..(i + 1) * nb_bits]);
            e_prime_minus_sum_batch.push(self.fcom.affine_add_cst(-sum, e_prime_mac_batch[i]));
        }
        print!("CHECK_Z< ... ");
        let start = Instant::now();
        self.fcom
            .check_zero(channel, rng, &e_prime_minus_sum_batch)?;
        println!("CHECK_Z> {:?}", start.elapsed());

        Ok(())
    }

    /// conversion checking
    pub fn conv<C: AbstractChannel, RNG: CryptoRng + Rng>(
        &mut self,
        channel: &mut C,
        rng: &mut RNG,
        num_bucket: usize,
        num_cut: usize,
        edabits_vector_mac: &[EdabitsVerifier<FE>],
        bucket_channels: Option<Vec<SyncChannel<BufReader<TcpStream>, BufWriter<TcpStream>>>>,
        with_quicksilver: bool,
    ) -> Result<(), Error> {
        let n = edabits_vector_mac.len();
        let nb_bits = edabits_vector_mac[0].bits.len();
        let nb_random_edabits = n * num_bucket + num_cut;
        let nb_random_dabits = n * num_bucket;

        let phase1 = Instant::now();
        // step 1)a)
        print!("Step 1)a) RANDOM EDABITS ... ");
        let start = Instant::now();
        let mut r_mac = self.random_edabits(channel, rng, nb_bits, nb_random_edabits)?;
        println!("{:?}", start.elapsed());

        // step 1)b)
        print!("Step 1)b) RANDOM DABITS ... ");
        let start = Instant::now();
        let mut dabits_mac = self.random_dabits(channel, rng, nb_random_dabits)?;
        println!("{:?}", start.elapsed());

        // step 1)c):
        print!("Step 1)c) RANDOM TRIPLES ... ");
        let mut random_triples = Vec::new();
        let start = Instant::now();
        if !with_quicksilver {
            // with wolverine
            let how_many = num_bucket * n * nb_bits + num_cut * nb_bits;
            self.random_triples(channel, rng, how_many, &mut random_triples)?;
        }
        println!("{:?}", start.elapsed());

        // step 2)
        print!("Step 2) CHECK DABITS ... ");
        let start = Instant::now();
        self.fdabit(channel, rng, &dabits_mac)?;
        println!("{:?}", start.elapsed());

        // step 3): get seed for permutation
        let seed = rng.gen::<Block>();
        channel.write_block(&seed)?;
        channel.flush()?;
        let mut shuffle_rng = AesRng::from_seed(seed);

        // step 4): shuffle the edabits, dabits, triples
        print!("Step 4) SHUFFLE ... ");
        let start = Instant::now();
        generate_permutation(&mut shuffle_rng, &mut r_mac);
        generate_permutation(&mut shuffle_rng, &mut dabits_mac);
        generate_permutation(&mut shuffle_rng, &mut random_triples);
        println!("{:?}", start.elapsed());

        // step 5)a):
        print!("Step 5)a) OPEN edabits ... ");
        let start = Instant::now();
        let base = n * num_bucket;
        let mut a_vec = Vec::with_capacity(nb_bits);
        let mut a_m = Vec::with_capacity(1);
        for i in 0..num_cut {
            let idx = base + i;
            let a_mac = &r_mac[idx];
            self.fcom_f2.open(channel, &a_mac.bits, &mut a_vec)?;
            self.fcom.open(channel, &[a_mac.value], &mut a_m)?;
            if convert_bits_to_field::<FE::PrimeField>(&a_vec) != a_m[0] {
                return Err(Error::Other("Wrong open random edabit".to_string()));
            }
        }
        println!("{:?}", start.elapsed());

        // step 5) b):
        print!("Step 5)b) OPEN triples ... ");
        let start = Instant::now();
        if !with_quicksilver {
            let mut res = Vec::with_capacity(2);
            let base = n * num_bucket * nb_bits;
            for i in 0..num_cut * nb_bits {
                let (x_mac, y_mac, z_mac) = random_triples[base + i];
                self.fcom_f2.open(channel, &[x_mac, y_mac], &mut res)?;
                let x = res[0];
                let y = res[1];
                let v = self.fcom_f2.affine_add_cst(-(x * y), z_mac);
                self.fcom_f2.check_zero(channel, rng, &[v])?;
            }
        }
        println!("{:?}", start.elapsed());

        println!("Total Steps 1-2-3-4-5: {:?}", phase1.elapsed());

        let phase2 = Instant::now();
        // step 6)
        println!("step 6)a-e) bitADDcarry etc: ... ");

        if bucket_channels.is_none() {
            let mut convert_bit_2_field_aux1 = Vec::with_capacity(n);
            let mut convert_bit_2_field_aux2 = Vec::with_capacity(n);
            let mut e_m_batch = Vec::with_capacity(n);
            let mut ei_batch = Vec::with_capacity(n);
            for j in 0..num_bucket {
                // base index for the window of `idx_base..idx_base + n` values
                let idx_base = j * n;

                if with_quicksilver {
                    self.conv_loop(
                        channel,
                        rng,
                        &edabits_vector_mac,
                        &r_mac[idx_base..idx_base + n],
                        &dabits_mac[idx_base..idx_base + n],
                        &mut convert_bit_2_field_aux1,
                        &mut convert_bit_2_field_aux2,
                        &mut e_m_batch,
                        &mut ei_batch,
                        &Vec::new(),
                    )?;
                } else {
                    self.conv_loop(
                        channel,
                        rng,
                        &edabits_vector_mac,
                        &r_mac[idx_base..idx_base + n],
                        &dabits_mac[idx_base..idx_base + n],
                        &mut convert_bit_2_field_aux1,
                        &mut convert_bit_2_field_aux2,
                        &mut e_m_batch,
                        &mut ei_batch,
                        &random_triples[idx_base * nb_bits..idx_base * nb_bits + n * nb_bits],
                    )?;
                }
            }
        } else {
            let mut j = 0;
            let mut handles = Vec::new();
            for mut bucket_channel in bucket_channels.unwrap().into_iter() {
                // base index for the window of `idx_base..idx_base + n` values
                let idx_base = j * n;

                // splitting the vectors to spawn
                let mut edabits_vector_mac_par = Vec::with_capacity(n);
                for edabits in edabits_vector_mac.iter() {
                    edabits_vector_mac_par.push(copy_edabits_verifier(edabits));
                }

                let mut r_mac_par = Vec::with_capacity(n);
                for r_elm in r_mac[idx_base..idx_base + n].iter() {
                    r_mac_par.push(copy_edabits_verifier(r_elm));
                }

                let mut dabits_mac_par = Vec::with_capacity(n);
                for elm in dabits_mac[idx_base..idx_base + n].iter() {
                    dabits_mac_par.push(elm.clone());
                }

                let mut random_triples_par = Vec::new(); //with_capacity(n * nb_bits);
                if !with_quicksilver {
                    //let mut random_triples_par = Vec::with_capacity(n * nb_bits);
                    for elm in
                        random_triples[idx_base * nb_bits..idx_base * nb_bits + n * nb_bits].iter()
                    {
                        random_triples_par.push(elm.clone());
                    }
                }

                let mut new_verifier = self.duplicate(channel, rng)?;
                let handle = std::thread::spawn(move || {
                    let mut convert_bit_2_field_aux1 = Vec::with_capacity(n);
                    let mut convert_bit_2_field_aux2 = Vec::with_capacity(n);
                    let mut e_m_batch = Vec::with_capacity(n);
                    let mut ei_batch = Vec::with_capacity(n);
                    new_verifier.conv_loop(
                        &mut bucket_channel,
                        &mut AesRng::new(),
                        &edabits_vector_mac_par,
                        &r_mac_par,
                        &dabits_mac_par,
                        &mut convert_bit_2_field_aux1,
                        &mut convert_bit_2_field_aux2,
                        &mut e_m_batch,
                        &mut ei_batch,
                        &random_triples_par,
                    )
                });
                handles.push(handle);

                j += 1;
            }

            for handle in handles {
                handle.join().unwrap().unwrap();
            }
        }
        println!("step 6)a-e) bitADDcarry etc: {:?}", phase2.elapsed());

        Ok(())
    }
}

#[cfg(test)]
mod tests {

    use super::super::homcom::{MacProver, MacVerifier};
    use super::{
        f2_to_fe, DabitProver, DabitVerifier, EdabitsProver, EdabitsVerifier, ProverConv,
        VerifierConv,
    };
    use crate::svole::wykw::{LPN_EXTEND_SMALL, LPN_SETUP_SMALL};
    use scuttlebutt::ring::FiniteRing;
    use scuttlebutt::{
        field::{F61p, FiniteField, F2},
        AesRng, Channel,
    };
    use std::{
        io::{BufReader, BufWriter},
    };
    use uds_windows::UnixStream;
    
    const DEFAULT_NUM_BUCKET: usize = 5;
    const DEFAULT_NUM_CUT: usize = 5;
    const NB_BITS: usize = 38;

    fn test_convert_bit_2_field<FE: FiniteField<PrimeField = FE>>() -> () {
        let count = 100;
        let (sender, receiver) = UnixStream::pair().unwrap();
        let handle = std::thread::spawn(move || {
            let mut rng = AesRng::new();
            let reader = BufReader::new(sender.try_clone().unwrap());
            let writer = BufWriter::new(sender);
            let mut channel = Channel::new(reader, writer);
            let mut fconv =
                ProverConv::<FE>::init(&mut channel, &mut rng, LPN_SETUP_SMALL, LPN_EXTEND_SMALL)
                    .unwrap();

            let mut res = Vec::new();
            for _ in 0..count {
                let MacProver(rb, rb_mac) = fconv.fcom_f2.random(&mut channel, &mut rng).unwrap();
                let rm = f2_to_fe(rb);
                let rm_mac = fconv.fcom.input(&mut channel, &mut rng, &[rm]).unwrap()[0];
                let MacProver(x_f2, x_f2_mac) =
                    fconv.fcom_f2.random(&mut channel, &mut rng).unwrap();

                let mut convert_bit_2_field_aux = Vec::new();
                let mut x_m_batch = Vec::new();
                fconv
                    .convert_bit_2_field(
                        &mut channel,
                        &[DabitProver {
                            bit: MacProver(rb, rb_mac),
                            value: MacProver(rm, rm_mac),
                        }],
                        &[MacProver(x_f2, x_f2_mac)],
                        &mut convert_bit_2_field_aux,
                        &mut x_m_batch,
                    )
                    .unwrap();

                let _ = fconv.fcom.open(&mut channel, &x_m_batch).unwrap();
                assert_eq!(f2_to_fe::<FE::PrimeField>(x_f2), x_m_batch[0].0);
                res.push((x_f2, x_m_batch[0].0));
            }
            res
        });
        let mut rng = AesRng::new();
        let reader = BufReader::new(receiver.try_clone().unwrap());
        let writer = BufWriter::new(receiver);
        let mut channel = Channel::new(reader, writer);
        let mut fconv =
            VerifierConv::<FE>::init(&mut channel, &mut rng, LPN_SETUP_SMALL, LPN_EXTEND_SMALL)
                .unwrap();

        let mut res = Vec::new();
        for _ in 0..count {
            let rb_mac = fconv.fcom_f2.random(&mut channel, &mut rng).unwrap();
            let r_m_mac = fconv.fcom.input(&mut channel, &mut rng, 1).unwrap()[0];
            let x_f2_mac = fconv.fcom_f2.random(&mut channel, &mut rng).unwrap();

            let mut convert_bit_2_field_aux1 = Vec::new();
            let mut convert_bit_2_field_aux2 = Vec::new();
            let mut x_m_batch = Vec::new();
            fconv
                .convert_bit_2_field(
                    &mut channel,
                    &[DabitVerifier {
                        bit: rb_mac,
                        value: r_m_mac,
                    }],
                    &[x_f2_mac],
                    &mut convert_bit_2_field_aux1,
                    &mut convert_bit_2_field_aux2,
                    &mut x_m_batch,
                )
                .unwrap();

            let mut x_m = Vec::new();
            fconv
                .fcom
                .open(&mut channel, &[x_m_batch[0]], &mut x_m)
                .unwrap();
            res.push(x_m[0]);
        }

        let resprover = handle.join().unwrap();

        for i in 0..count {
            assert_eq!(resprover[i].1, res[i]);
        }
    }

    fn test_bit_add_carry<FE: FiniteField<PrimeField = FE>>() -> () {
        let power = 6;
        let (sender, receiver) = UnixStream::pair().unwrap();

        // adding
        //   110101
        //   101110
        // --------
        //  1100011
        let x = vec![F2::ONE, F2::ZERO, F2::ONE, F2::ZERO, F2::ONE, F2::ONE];
        let y = vec![F2::ZERO, F2::ONE, F2::ONE, F2::ONE, F2::ZERO, F2::ONE];
        let expected = vec![F2::ONE, F2::ONE, F2::ZERO, F2::ZERO, F2::ZERO, F2::ONE];
        let carry = F2::ONE;

        let handle = std::thread::spawn(move || {
            let mut rng = AesRng::new();
            let reader = BufReader::new(sender.try_clone().unwrap());
            let writer = BufWriter::new(sender);
            let mut channel = Channel::new(reader, writer);
            let mut fconv =
                ProverConv::<FE>::init(&mut channel, &mut rng, LPN_SETUP_SMALL, LPN_EXTEND_SMALL)
                    .unwrap();

            let x_mac = fconv.fcom_f2.input(&mut channel, &mut rng, &x).unwrap();
            let y_mac = fconv.fcom_f2.input(&mut channel, &mut rng, &y).unwrap();

            let mut vx = Vec::new();
            for i in 0..power {
                vx.push(MacProver(x[i], x_mac[i]));
            }

            let mut vy = Vec::new();
            for i in 0..power {
                vy.push(MacProver(y[i], y_mac[i]));
            }
            let default_fe = MacProver(FE::PrimeField::ZERO, FE::ZERO);
            let (res, c) = fconv
                .bit_add_carry(
                    &mut channel,
                    &mut rng,
                    &[EdabitsProver {
                        bits: vx,
                        value: default_fe,
                    }],
                    &[EdabitsProver {
                        bits: vy,
                        value: default_fe,
                    }],
                    vec![].as_slice(),
                )
                .unwrap()[0]
                .clone();

            fconv.fcom_f2.open(&mut channel, &res).unwrap();

            fconv.fcom_f2.open(&mut channel, &[c]).unwrap();
            (res, c)
        });
        let mut rng = AesRng::new();
        let reader = BufReader::new(receiver.try_clone().unwrap());
        let writer = BufWriter::new(receiver);
        let mut channel = Channel::new(reader, writer);
        let mut fconv =
            VerifierConv::<FE>::init(&mut channel, &mut rng, LPN_SETUP_SMALL, LPN_EXTEND_SMALL)
                .unwrap();

        let x_mac = fconv.fcom_f2.input(&mut channel, &mut rng, power).unwrap();
        let y_mac = fconv.fcom_f2.input(&mut channel, &mut rng, power).unwrap();

        let default_fe = MacVerifier(FE::ZERO);
        let (res_mac, c_mac) = fconv
            .bit_add_carry(
                &mut channel,
                &mut rng,
                &[EdabitsVerifier {
                    bits: x_mac,
                    value: default_fe,
                }],
                &[EdabitsVerifier {
                    bits: y_mac,
                    value: default_fe,
                }],
                vec![].as_slice(),
            )
            .unwrap()[0]
            .clone();

        let mut res = Vec::new();
        fconv
            .fcom_f2
            .open(&mut channel, &res_mac, &mut res)
            .unwrap();

        let mut c = Vec::new();
        fconv.fcom_f2.open(&mut channel, &[c_mac], &mut c).unwrap();

        let _resprover = handle.join().unwrap();

        for i in 0..power {
            assert_eq!(expected[i], res[i]);
        }
        assert_eq!(carry, c[0]);
    }

    fn test_fdabit<FE: FiniteField<PrimeField = FE>>() -> () {
        let count = 100;
        let (sender, receiver) = UnixStream::pair().unwrap();
        let handle = std::thread::spawn(move || {
            let mut rng = AesRng::new();
            let reader = BufReader::new(sender.try_clone().unwrap());
            let writer = BufWriter::new(sender);
            let mut channel = Channel::new(reader, writer);
            let mut fconv =
                ProverConv::<FE>::init(&mut channel, &mut rng, LPN_SETUP_SMALL, LPN_EXTEND_SMALL)
                    .unwrap();

            let dabits = fconv.random_dabits(&mut channel, &mut rng, count).unwrap();
            let _ = fconv.fdabit(&mut channel, &mut rng, &dabits).unwrap();
            ()
        });
        let mut rng = AesRng::new();
        let reader = BufReader::new(receiver.try_clone().unwrap());
        let writer = BufWriter::new(receiver);
        let mut channel = Channel::new(reader, writer);
        let mut fconv =
            VerifierConv::<FE>::init(&mut channel, &mut rng, LPN_SETUP_SMALL, LPN_EXTEND_SMALL)
                .unwrap();

        let dabits_mac = fconv.random_dabits(&mut channel, &mut rng, count).unwrap();
        let _ = fconv.fdabit(&mut channel, &mut rng, &dabits_mac).unwrap();

        handle.join().unwrap();
    }

    fn test_conv<FE: FiniteField<PrimeField = FE>>() -> () {
        let nb_edabits = 50;
        let with_quicksilver = true;
        let (sender, receiver) = UnixStream::pair().unwrap();

        let handle = std::thread::spawn(move || {
            let mut rng = AesRng::new();
            let reader = BufReader::new(sender.try_clone().unwrap());
            let writer = BufWriter::new(sender);
            let mut channel = Channel::new(reader, writer);
            let mut fconv =
                ProverConv::<FE>::init(&mut channel, &mut rng, LPN_SETUP_SMALL, LPN_EXTEND_SMALL)
                    .unwrap();

            for n in 1..nb_edabits {
                let edabits = fconv
                    .random_edabits(&mut channel, &mut rng, NB_BITS, n)
                    .unwrap();

                let _ = fconv
                    .conv(
                        &mut channel,
                        &mut rng,
                        DEFAULT_NUM_BUCKET,
                        DEFAULT_NUM_CUT,
                        &edabits,
                        None,
                        with_quicksilver,
                    )
                    .unwrap();
            }
            ()
        });
        let mut rng = AesRng::new();
        let reader = BufReader::new(receiver.try_clone().unwrap());
        let writer = BufWriter::new(receiver);
        let mut channel = Channel::new(reader, writer);
        let mut fconv =
            VerifierConv::<FE>::init(&mut channel, &mut rng, LPN_SETUP_SMALL, LPN_EXTEND_SMALL)
                .unwrap();

        let mut res = Vec::new();
        for n in 1..nb_edabits {
            let edabits = fconv
                .random_edabits(&mut channel, &mut rng, NB_BITS, n)
                .unwrap();

            let r = fconv
                .conv(
                    &mut channel,
                    &mut rng,
                    DEFAULT_NUM_BUCKET,
                    DEFAULT_NUM_CUT,
                    &edabits,
                    None,
                    with_quicksilver,
                )
                .unwrap();
            res.push(r);
        }

        let _resprover = handle.join().unwrap();
        ()
    }

    #[test]
    fn test_convert_bit_2_field_f61p() {
        test_convert_bit_2_field::<F61p>();
    }

    #[test]
    fn test_bit_add_carry_f61p() {
        test_bit_add_carry::<F61p>();
    }

    #[test]
    fn test_fdabit_f61p() {
        test_fdabit::<F61p>();
    }

    #[test]
    fn test_conv_f61p() {
        test_conv::<F61p>();
    }
}
