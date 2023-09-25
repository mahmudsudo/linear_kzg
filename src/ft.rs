// Note: a lot of this file is copypasta from zkcrypto/bellman

use std::ops::{AddAssign, MulAssign, SubAssign};

use crate::polynomial::Polynomial;
use crate::KZGError;
use blstrs::Scalar;
use pairing::group::ff::Field;
use pairing::group::ff::PrimeField;

#[cfg(feature = "parallel")]
use crate::utils::chunk_by_num_threads;
#[cfg(feature = "parallel")]
use crate::utils::log2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvaluationDomain {
    pub(crate) coeffs: Vec<Scalar>,
    pub(crate) d: usize,
    pub(crate) exp: u32,
    pub(crate) omega: Scalar,
    pub(crate) omegainv: Scalar,
    pub(crate) geninv: Scalar,
    pub(crate) minv: Scalar,
}

impl From<EvaluationDomain> for Polynomial {
    fn from(domain: EvaluationDomain) -> Polynomial {
        Polynomial::new(domain.coeffs)
    }
}

impl AsRef<[Scalar]> for EvaluationDomain {
    fn as_ref(&self) -> &[Scalar] {
        &self.coeffs
    }
}

impl AsMut<[Scalar]> for EvaluationDomain {
    fn as_mut(&mut self) -> &mut [Scalar] {
        &mut self.coeffs
    }
}

impl EvaluationDomain {
    pub fn into_coeffs(self) -> Vec<Scalar> {
        self.coeffs
    }

    pub fn len(&self) -> usize {
        self.coeffs.len()
    }

    // returns m, exp, and omega
    pub fn compute_omega(d: usize) -> Result<(usize, u32, Scalar), KZGError> {
        // Compute the size of our evaluation domain
        let mut m = 1;
        let mut exp = 0;

        // TODO cache this in a lazy static
        while m < d {
            m *= 2;
            exp += 1;

            // The pairing-friendly curve may not be able to support
            // large enough (radix2) evaluation domains.
            if exp >= Scalar::S {
                return Err(KZGError::PolynomialDegreeTooLarge);
            }
        }

        // Compute omega, the 2^exp primitive root of unity
        let omega = Scalar::root_of_unity().pow_vartime(&[1 << (Scalar::S - exp)]);

        Ok((m, exp, omega))
    }

    pub fn clone_with_different_coeffs(&self, coeffs: Vec<Scalar>) -> EvaluationDomain {
        EvaluationDomain { coeffs, ..*self }
    }

    pub fn new(coeffs: Vec<Scalar>, d: usize, exp: u32, omega: Scalar) -> Self {
        EvaluationDomain {
            coeffs,
            d,
            exp,
            omega,
            omegainv: omega.invert().unwrap(),
            geninv: Scalar::multiplicative_generator().invert().unwrap(),
            minv: Scalar::from(d as u64).invert().unwrap(),
        }
    }

    pub fn from_coeffs(mut coeffs: Vec<Scalar>) -> Result<EvaluationDomain, KZGError> {
        let (m, exp, omega) = Self::compute_omega(coeffs.len())?;

        // Extend the coeffs vector with zeroes if necessary
        coeffs.resize(m, Scalar::zero());

        Ok(EvaluationDomain {
            d: m,
            coeffs,
            exp,
            omega,
            omegainv: omega.invert().unwrap(),
            geninv: Scalar::multiplicative_generator().invert().unwrap(),
            minv: Scalar::from(m as u64).invert().unwrap(),
        })
    }

    pub fn fft(&mut self) {
        best_fft(&mut self.coeffs, &self.omega, self.exp);
    }

    pub fn ifft(&mut self) {
        best_fft(&mut self.coeffs, &self.omegainv, self.exp);

        #[cfg(feature = "parallel")]
        rayon::scope(|scope| {
            let minv = self.minv;

            let chunk_size = chunk_by_num_threads(self.coeffs.len());

            for v in self.coeffs.chunks_mut(chunk_size) {
                scope.spawn(move |_scope| {
                    for v in v {
                        v.mul_assign(&minv);
                    }
                });
            }
        });

        #[cfg(not(feature = "parallel"))]
        {
            let minv = self.minv;
            for v in self.coeffs.iter_mut() {
                v.mul_assign(&minv);
            }
        }
    }

    pub fn distribute_powers(&mut self, g: Scalar) {
        #[cfg(feature = "parallel")]
        rayon::scope(|scope| {
            let chunk_size = chunk_by_num_threads(self.coeffs.len());

            for (i, v) in self.coeffs.chunks_mut(chunk_size).enumerate() {
                scope.spawn(move |_scope| {
                    let mut u = g.pow_vartime(&[(i * chunk_size) as u64]);
                    for v in v.iter_mut() {
                        v.mul_assign(&u);
                        u.mul_assign(&g);
                    }
                });
            }
        });

        #[cfg(not(feature = "parallel"))]
        {
            for (i, v) in self.coeffs.iter_mut().enumerate() {
                let mut u = g.pow_vartime(&[i as u64]);
                v.mul_assign(&u);
                u.mul_assign(&g);
            }
        };
    }

    pub fn coset_fft(&mut self) {
        self.distribute_powers(Scalar::multiplicative_generator());
        self.fft();
    }

    pub fn icoset_fft(&mut self) {
        let geninv = self.geninv;

        self.ifft();
        self.distribute_powers(geninv);
    }

    /// This evaluates t(tau) for this domain, which is
    /// tau^m - 1 for these radix-2 domains.
    pub fn z(&self, tau: &Scalar) -> Scalar {
        let mut tmp = tau.pow_vartime(&[self.coeffs.len() as u64]);
        tmp.sub_assign(&Scalar::one());

        tmp
    }

    /// The target polynomial is the zero polynomial in our
    /// evaluation domain, so we must perform division over
    /// a coset.
    pub fn divide_by_z_on_coset(&mut self) {
        let i = self
            .z(&Scalar::multiplicative_generator())
            .invert()
            .unwrap();

        #[cfg(feature = "parallel")]
        rayon::scope(|scope| {
            let chunk_size = chunk_by_num_threads(self.coeffs.len());

            for v in self.coeffs.chunks_mut(chunk_size) {
                scope.spawn(move |_scope| {
                    for v in v {
                        v.mul_assign(&i);
                    }
                });
            }
        });

        #[cfg(not(feature = "parallel"))]
        {
            for v in self.coeffs.iter_mut() {
                v.mul_assign(&i);
            }
        }
    }

    /// Perform O(n) multiplication of two polynomials in the domain.
    pub fn mul_assign(&mut self, other: &EvaluationDomain) {
        assert_eq!(self.coeffs.len(), other.coeffs.len());

        #[cfg(feature = "parallel")]
        rayon::scope(|scope| {
            let chunk_size = chunk_by_num_threads(self.coeffs.len());

            for (a, b) in self
                .coeffs
                .chunks_mut(chunk_size)
                .zip(other.coeffs.chunks(chunk_size))
            {
                scope.spawn(move |_scope| {
                    for (a, b) in a.iter_mut().zip(b.iter()) {
                        a.mul_assign(b);
                    }
                });
            }
        });

        #[cfg(not(feature = "parallel"))]
        for (a, b) in self.coeffs.iter_mut().zip(other.coeffs.iter()) {
            a.mul_assign(b);
        }
    }

    /// Perform O(n) subtraction of one polynomial from another in the domain.
    pub fn sub_assign(&mut self, other: &EvaluationDomain) {
        assert_eq!(self.coeffs.len(), other.coeffs.len());

        #[cfg(feature = "parallel")]
        rayon::scope(|scope| {
            let chunk_size = chunk_by_num_threads(self.coeffs.len());

            for (a, b) in self
                .coeffs
                .chunks_mut(chunk_size)
                .zip(other.coeffs.chunks(chunk_size))
            {
                scope.spawn(move |_scope| {
                    for (a, b) in a.iter_mut().zip(b.iter()) {
                        a.sub_assign(b);
                    }
                });
            }
        });

        #[cfg(not(feature = "parallel"))]
        for (a, b) in self.coeffs.iter_mut().zip(other.coeffs.iter()) {
            a.sub_assign(b);
        }
    }
}

fn best_fft(a: &mut [Scalar], omega: &Scalar, log_n: u32) {
    #[cfg(feature = "parallel")]
    {
        let log_cpus = log2(rayon::current_num_threads() as u64) as u32;

        if log_n <= log_cpus {
            serial_fft(a, omega, log_n);
        } else {
            parallel_fft(a, omega, log_n, log_cpus);
        }
    }

    #[cfg(not(feature = "parallel"))]
    serial_fft(a, omega, log_n);
}

#[allow(clippy::many_single_char_names)]
fn serial_fft(a: &mut [Scalar], omega: &Scalar, log_n: u32) {
    fn bitreverse(mut n: u32, l: u32) -> u32 {
        let mut r = 0;
        for _ in 0..l {
            r = (r << 1) | (n & 1);
            n >>= 1;
        }
        r
    }

    let n = a.len() as u32;
    assert_eq!(n, 1 << log_n);

    for k in 0..n {
        let rk = bitreverse(k, log_n);
        if k < rk {
            a.swap(rk as usize, k as usize);
        }
    }

    let mut m = 1;
    for _ in 0..log_n {
        let w_m = omega.pow_vartime(&[u64::from(n / (2 * m))]);

        let mut k = 0;
        while k < n {
            let mut w = Scalar::one();
            for j in 0..m {
                let mut t = a[(k + j + m) as usize];
                t.mul_assign(&w);
                let mut tmp = a[(k + j) as usize];
                tmp.sub_assign(&t);
                a[(k + j + m) as usize] = tmp;
                a[(k + j) as usize].add_assign(&t);
                w.mul_assign(&w_m);
            }

            k += 2 * m;
        }

        m *= 2;
    }
}

#[cfg(feature = "parallel")]
fn parallel_fft(a: &mut [Scalar], omega: &Scalar, log_n: u32, log_cpus: u32) {
    assert!(log_n >= log_cpus);

    let num_cpus = 1 << log_cpus;
    let log_new_n = log_n - log_cpus;
    let mut tmp = vec![vec![Scalar::zero(); 1 << log_new_n]; num_cpus];
    let new_omega = omega.pow_vartime(&[num_cpus as u64]);

    rayon::scope(|scope| {
        let a = &*a;

        for (j, tmp) in tmp.iter_mut().enumerate() {
            scope.spawn(move |_scope| {
                // Shuffle into a sub-FFT
                let omega_j = omega.pow_vartime(&[j as u64]);
                let omega_step = omega.pow_vartime(&[(j as u64) << log_new_n]);

                let mut elt = Scalar::one();
                for (i, tmp) in tmp.iter_mut().enumerate() {
                    for s in 0..num_cpus {
                        let idx = (i + (s << log_new_n)) % (1 << log_n);
                        let mut t = a[idx];
                        t.mul_assign(&elt);
                        tmp.add_assign(&t);
                        elt.mul_assign(&omega_step);
                    }
                    elt.mul_assign(&omega_j);
                }

                // Perform sub-FFT
                serial_fft(tmp, &new_omega, log_new_n);
            });
        }
    });

    // TODO: does this hurt or help?
    rayon::scope(|scope| {
        let chunk_size = chunk_by_num_threads(a.len());
        let tmp = &tmp;

        for (idx, a) in a.chunks_mut(chunk_size).enumerate() {
            scope.spawn(move |_scope| {
                let mut idx = idx * chunk_size;
                let mask = (1 << log_cpus) - 1;
                for a in a {
                    *a = tmp[idx & mask][idx >> log_cpus];
                    idx += 1;
                }
            });
        }
    });
}

#[cfg(all(feature = "serde_support", feature = "b12_381"))]
use crate::wrapper_types::SerializablePrimeField;

#[cfg(all(feature = "serde_support", feature = "b12_381"))]
use bls12_381::Scalar;

#[cfg(all(feature = "serde_support", feature = "b12_381"))]
#[derive(Serialize, Deserialize)]
pub struct SerializableEvaluationDomain {
    coeffs: Vec<SerializablePrimeField<Scalar>>,
    exp: u32,
    omega: SerializablePrimeField<Scalar>,
    omegainv: SerializablePrimeField<Scalar>,
    geninv: SerializablePrimeField<Scalar>,
    minv: SerializablePrimeField<Scalar>,
}

#[cfg(test)]
use rand::{rngs::SmallRng, Rng, SeedableRng};

// Test multiplying various (low degree) polynomials together and
// comparing with naive evaluations.
#[test]
fn polynomial_arith() {
    fn test_mul<R: Rng>(mut rng: &mut R) {
        for coeffs_a in vec![1, 5, 10, 50] {
            for coeffs_b in vec![1, 5, 10, 50] {
                let a: Vec<_> = (0..coeffs_a).map(|_| Scalar::random(&mut rng)).collect();
                let b: Vec<_> = (0..coeffs_b).map(|_| Scalar::random(&mut rng)).collect();

                let a = Polynomial::new_from_coeffs(a, coeffs_a - 1);
                let b = Polynomial::new_from_coeffs(b, coeffs_b - 1);

                // naive evaluation
                let naive = a.clone() * b.clone();
                let fft = a.fft_mul(&b);

                assert!(naive == fft);
            }
        }
    }

    let rng = &mut SmallRng::from_seed([42; 32]);

    test_mul(rng);
}

#[cfg(test)]
fn random_evals(rng: &mut SmallRng, d: usize) -> EvaluationDomain {
    let mut coeffs = vec![Scalar::zero(); d];

    for i in 0..d {
        coeffs[i] = rng.gen::<u64>().into();
    }

    EvaluationDomain::from_coeffs(coeffs).unwrap()
}

#[test]
fn fft_composition() {
    use rand::RngCore;

    fn test_comp<R: RngCore>(mut rng: &mut R) {
        for coeffs in 0..10 {
            let coeffs = 1 << coeffs;

            let mut v = vec![];
            for _ in 0..coeffs {
                v.push(Scalar::random(&mut rng));
            }

            let mut domain = EvaluationDomain::from_coeffs(v.clone()).unwrap();
            domain.ifft();
            domain.fft();
            assert!(v == domain.coeffs);
            domain.fft();
            domain.ifft();
            assert!(v == domain.coeffs);
            domain.icoset_fft();
            domain.coset_fft();
            assert!(v == domain.coeffs);
            domain.coset_fft();
            domain.icoset_fft();
            assert!(v == domain.coeffs);
        }
    }

    let rng = &mut rand::thread_rng();

    test_comp(rng);
}

#[cfg(feature = "parallel")]
#[test]
fn parallel_fft_consistency() {
    use rand::RngCore;
    use std::cmp::min;

    fn test_consistency<R: RngCore>(mut rng: &mut R) {
        for _ in 0..5 {
            for log_d in 0..10 {
                let d = 1 << log_d;

                let v1 = (0..d).map(|_| Scalar::random(&mut rng)).collect::<Vec<_>>();
                let mut v1 = EvaluationDomain::from_coeffs(v1).unwrap();
                let mut v2 = EvaluationDomain::from_coeffs(v1.coeffs.clone()).unwrap();

                for log_cpus in log_d..min(log_d + 1, 3) {
                    parallel_fft(&mut v1.coeffs, &v1.omega, log_d, log_cpus);
                    serial_fft(&mut v2.coeffs, &v2.omega, log_d);

                    assert!(v1.coeffs == v2.coeffs);
                }
            }
        }
    }

    let rng = &mut rand::thread_rng();

    test_consistency(rng);
}
