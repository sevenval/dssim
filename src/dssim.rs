#![allow(non_upper_case_globals)]
#![allow(non_snake_case)]
/*
 * © 2011-2017 Kornel Lesiński. All rights reserved.
 *
 * This file is part of DSSIM.
 *
 * DSSIM is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License
 * as published by the Free Software Foundation, either version 3
 * of the License, or (at your option) any later version.
 *
 * DSSIM is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU Affero General Public License for more details.
 *
 * You should have received a copy of the license along with DSSIM.
 * If not, see <http://www.gnu.org/licenses/agpl.txt>.
 */

extern crate itertools;
extern crate imgref;

use self::itertools::multizip;
use blur;
use image::*;
use imgref::*;
use rayon;
use rayon::prelude::*;
use std;
use std::ops;
pub use val::Dssim as Val;
pub use tolab::ToLABBitmap;

trait Channable<T, I> {
    fn img1_img2_blur<'a>(&self, modified: &mut Self, tmp: &mut [I]) -> Vec<T>;
}

struct DssimChan<T> {
    pub width: usize,
    pub height: usize,
    pub img: Option<Vec<T>>,
    pub mu: Vec<T>,
    pub img_sq_blur: Vec<T>,
    pub is_chroma: bool,
}

/// Configuration for the comparison
pub struct Dssim {
    scale_weights: Vec<f64>,
    save_maps_scales: u8,
}

struct DssimChanScale<T> {
    chan: Vec<DssimChan<T>>,
}

/// Abstract wrapper for images. See `Dssim.create_image()`
pub struct DssimImage<T> {
    scale: Vec<DssimChanScale<T>>,
}

// Scales are taken from IW-SSIM, but this is not IW-SSIM algorithm
const DEFAULT_WEIGHTS: [f64; 5] = [0.0448, 0.2856, 0.3001, 0.2363, 0.1333];

/// Detailed comparison result
pub struct SsimMap {
    /// SSIM scores
    pub map: ImgVec<f32>,
    /// Average SSIM (not DSSIM)
    pub ssim: f64,
}

/// Create new context for a comparison
pub fn new() -> Dssim {
    Dssim::new()
}

impl<T> DssimChan<T> {
    pub fn new(bitmap: Vec<T>, width: usize, height: usize, is_chroma: bool) -> DssimChan<T> {
        DssimChan {
            img: Some(bitmap),
            mu: Vec::with_capacity(width * height),
            img_sq_blur: Vec::new(),
            width: width,
            height: height,
            is_chroma: is_chroma,
        }
    }
}

impl DssimChan<f32> {
    fn preprocess(&mut self, tmp: &mut [f32]) {
        let width = self.width;
        let height = self.height;
        let tmp = &mut tmp[0..width * height];

        let img = self.img.as_mut().unwrap();
        assert_eq!(img.len(), self.width * self.height);
        assert!(width > 1);
        assert!(height > 1);

        unsafe {
            if self.is_chroma {
                blur::blur_in_place(&mut img[..], tmp, width, height);
            }

            self.mu.reserve(self.width * self.height);
            self.mu.set_len(self.width * self.height);
            blur::blur(&img[..], tmp, &mut self.mu[..], width, height);

            self.img_sq_blur = img.iter().cloned().map(|i| i * i).collect();
            blur::blur_in_place(&mut self.img_sq_blur[..], tmp, width, height);
        }
    }
}

impl Channable<LAB, f32> for [DssimChan<f32>] {
    fn img1_img2_blur<'a>(&self, modified: &mut Self, tmp32: &mut [f32]) -> Vec<LAB> {

        let blurred:Vec<_> = self.iter().zip(modified.iter_mut()).map(|(o,m)|{
            o.img1_img2_blur(m, tmp32)
        }).collect();

        return multizip((blurred[0].iter().cloned(), blurred[1].iter().cloned(), blurred[2].iter().cloned())).map(|(l,a,b)| {
            LAB {l,a,b}
        }).collect();
    }
}

impl Channable<f32, f32> for DssimChan<f32> {
    fn img1_img2_blur<'a>(&self, modified: &mut Self, tmp32: &mut [f32]) -> Vec<f32> {
        let mut modified_img = modified.img.take().unwrap();

        for (img2, img1) in modified_img.iter_mut().zip(self.img.as_ref().unwrap().iter()) {
            *img2 *= *img1
        }

        blur::blur_in_place(&mut modified_img[..], tmp32, self.width, self.height);
        return modified_img;
    }
}

impl Dssim {
    /// Create new context for comparisons
    pub fn new() -> Dssim {
        Dssim {
            scale_weights: DEFAULT_WEIGHTS[0..4].to_owned(),
            save_maps_scales: 0,
        }
    }

    /// Set how many scales will be used, and weights of each scale
    pub fn set_scales(&mut self, scales: &[f64]) {
        self.scale_weights = scales.to_vec();
    }

    /// Set how many scales will be kept for saving
    pub fn set_save_ssim_maps(&mut self, num_scales: u8) {
        self.save_maps_scales = num_scales;
    }

    fn create_scales<InBitmap, OutBitmap>(&self, src_img: &InBitmap) -> Vec<OutBitmap>
        where
        InBitmap: Downsample<Output=OutBitmap>,
        OutBitmap: Downsample<Output=OutBitmap>,
    {
        let num_scales = self.scale_weights.len();

        let mut downsampled: Vec<OutBitmap> = Vec::with_capacity(num_scales);
        for _ in 1..num_scales { // 1, because unscaled bitmap will be added
            let s = if let Some(l) = downsampled.last() {
                l.downsample()
            } else {
                src_img.downsample()
            };
            if let Some(s) = s {
                downsampled.push(s);
            } else {
                break;
            }
        }

        return downsampled;
    }

    /// The input image is defined using the `imgref` crate, and the pixel type can be:
    ///
    /// * `ImgVec<RGBAPLU>` — RGBA premultiplied alpha, float scaled to 0..1
    /// * `ImgVec<RGBLU>` — RGBA float scaled to 0..1
    /// * `ImgVec<f32>` — linear light grayscale, float scaled to 0..1
    ///
    /// And there's `ToRGBAPLU` (`.to_rgbaplu()`) helper to convert the input pixels from
    /// `[RGBA<u8>]`, `[RGBA<u16>]`, `[RGB<u8>]`, or `RGB<u16>`. See `main.rs` for example how it's done.
    ///
    /// You can implement `ToLABBitmap` and `Downsample` traits on your own image type.
    pub fn create_image<InBitmap, OutBitmap>(&self, src_img: &InBitmap) -> Option<DssimImage<f32>>
        where InBitmap: ToLABBitmap + Send + Sync + Downsample<Output = OutBitmap>,
              OutBitmap: ToLABBitmap + Send + Sync + Downsample<Output = OutBitmap>,
    {
        let (lab1, mut all_sizes) = rayon::join(|| {
            src_img.to_lab()
        }, || {
            self.create_scales(src_img).into_par_iter().map(|s| s.to_lab()).collect::<Vec<_>>()
        });

        all_sizes.insert(0, lab1);

        return Some(DssimImage {
            scale: all_sizes.into_par_iter().map(|s| {
                DssimChanScale {
                    chan: s.into_par_iter().enumerate().map(|(n,l)| {
                        let w = l.width();
                        let h = l.height();
                        let mut ch = DssimChan::new(l.buf, w, h, n > 0);

                        let mut tmp = {
                            let pixels = w * h;
                            let mut tmp = Vec::with_capacity(pixels);
                            unsafe { tmp.set_len(pixels) };
                            tmp
                        };

                        ch.preprocess(&mut tmp[..]);
                        ch
                    }).collect(),
                }
            }).collect(),
        });
    }

    /// Compare original with another image. See `create_image`
    ///
    /// The `SsimMap`s are returned only if you've enabled them first.
    ///
    /// `Val` is a fancy wrapper for `f64`
    pub fn compare(&mut self, original_image: &DssimImage<f32>, modified_image: DssimImage<f32>) -> (Val, Vec<SsimMap>) {
        let res: Vec<_> = self.scale_weights.par_iter().cloned().zip(
            modified_image.scale.into_par_iter().zip(original_image.scale.par_iter())
        ).enumerate().map(|(n, (weight, (mut modified_image_scale, original_image_scale)))| {
            let scale_width = original_image_scale.chan[0].width;
            let scale_height = original_image_scale.chan[0].height;
            let mut tmp = Vec::with_capacity(scale_width * scale_height);
            unsafe { tmp.set_len(scale_width * scale_height) };

            let ssim_map = match original_image_scale.chan.len() {
                3 => {
                    let (original_lab, (img1_img2_blur, modified_lab)) = rayon::join(
                    || Self::lab_chan(&original_image_scale),
                    || {
                        let img1_img2_blur = original_image_scale.chan.img1_img2_blur(&mut modified_image_scale.chan, &mut tmp[0 .. scale_width*scale_height]);
                        (img1_img2_blur, Self::lab_chan(&modified_image_scale))
                    });

                    Self::compare_channel(&original_lab, &modified_lab, &img1_img2_blur)
                },
                1 => {
                    let img1_img2_blur = original_image_scale.chan[0].img1_img2_blur(&mut modified_image_scale.chan[0], &mut tmp[0 .. scale_width*scale_height]);
                    Self::compare_channel(&original_image_scale.chan[0], &modified_image_scale.chan[0], &img1_img2_blur)
                },
                _ => panic!(),
            };

            let half = avgworst(ssim_map.as_ref());
            let half = avg(half.as_ref());
            let half = worst(half.as_ref());

            let sum = half.buf.iter().fold(0., |sum, i| sum + *i as f64);
            let score = sum / (half.buf.len() as f64);

            let map = if self.save_maps_scales as usize > n {
                Some(SsimMap {
                    map: ssim_map,
                    ssim: score,
                })
            } else {
                None
            };
            (score, weight, map)
        }).collect();

        let mut ssim_sum = 0.0;
        let mut weight_sum = 0.0;
        let mut ssim_maps = Vec::new();
        for (score, weight, map) in res {
            ssim_sum += score * weight;
            weight_sum += weight;
            if let Some(m) = map {
                ssim_maps.push(m);
            }
        }

        return (to_dssim(ssim_sum / weight_sum).into(), ssim_maps);
    }

    fn lab_chan(scale: &DssimChanScale<f32>) -> DssimChan<LAB> {
        let l = &scale.chan[0];
        let a = &scale.chan[1];
        let b = &scale.chan[2];
        assert_eq!(l.width, a.width);
        assert_eq!(b.width, a.width);
        DssimChan {
            img_sq_blur: multizip((l.img_sq_blur.iter().cloned(), a.img_sq_blur.iter().cloned(), b.img_sq_blur.iter().cloned()))
                .map(|(l,a,b)|LAB {l,a,b}).collect(),
            img: if let (&Some(ref l),&Some(ref a),&Some(ref b)) = (&l.img, &a.img, &b.img) {
                Some(multizip((l.iter().cloned(), a.iter().cloned(), b.iter().cloned())).map(|(l,a,b)|LAB {l,a,b}).collect())
            } else {None},
            mu: multizip((l.mu.iter().cloned(), a.mu.iter().cloned(), b.mu.iter().cloned())).map(|(l,a,b)|LAB {l,a,b}).collect(),
            is_chroma: false,
            width: l.width,
            height: l.height,
        }
    }

    fn compare_channel<L>(original: &DssimChan<L>, modified: &DssimChan<L>, img1_img2_blur: &[L]) -> ImgVec<f32>
        where L: Send + Sync + Clone + Copy + ops::Mul<Output = L> + ops::Sub<Output = L> + 'static,
              f32: std::convert::From<L>
    {
        assert_eq!(original.width, modified.width);
        assert_eq!(original.height, modified.height);

        let width = original.width;
        let height = original.height;

        let c1 = 0.01 * 0.01;
        let c2 = 0.03 * 0.03;

        let mut map_out = Vec::with_capacity(width * height);
        unsafe { map_out.set_len(width * height) };

        let mu_iter = original.mu.par_iter().cloned().zip_eq(modified.mu.par_iter().cloned());
        let sq_iter = original.img_sq_blur.par_iter().cloned().zip_eq(modified.img_sq_blur.par_iter().cloned());
        img1_img2_blur.par_iter().cloned().zip_eq(mu_iter).zip_eq(sq_iter).zip_eq(map_out.par_iter_mut())
        .for_each(|(((img1_img2_blur, (mu1, mu2)), (img1_sq_blur, img2_sq_blur)), map_out)| {
            let mu1mu1 = mu1 * mu1;
            let mu1mu2 = mu1 * mu2;
            let mu2mu2 = mu2 * mu2;
            let mu1_sq: f32 = mu1mu1.into();
            let mu2_sq: f32 = mu2mu2.into();
            let mu1_mu2: f32 = mu1mu2.into();
            let sigma1_sq: f32 = (img1_sq_blur - mu1mu1).into();
            let sigma2_sq: f32 = (img2_sq_blur - mu2mu2).into();
            let sigma12: f32 = (img1_img2_blur - mu1mu2).into();

            let ssim = (2. * mu1_mu2 + c1) * (2. * sigma12 + c2) /
                       ((mu1_sq + mu2_sq + c1) * (sigma1_sq + sigma2_sq + c2));

            *map_out = ssim;
        });

        return ImgVec::new(map_out, width, height);
    }
}

fn to_dssim(ssim: f64) -> f64 {
    debug_assert!(ssim > 0.0);
    return 1.0 / ssim.min(1.0) - 1.0;
}

#[test]
fn png_compare() {
    extern crate lodepng;

    use imgref::*;
    use linear::*;

    let mut d = new();
    let file1 = lodepng::decode32_file("tests/test1-sm.png").unwrap();
    let file2 = lodepng::decode32_file("tests/test2-sm.png").unwrap();

    let buf1 = &file1.buffer.to_rgbaplu()[..];
    let buf2 = &file2.buffer.to_rgbaplu()[..];
    let img1 = d.create_image(&Img::new(buf1, file1.width, file1.height)).unwrap();
    let img2 = d.create_image(&Img::new(buf2, file2.width, file2.height)).unwrap();

    let (res, _) = d.compare(&img1, img2);
    assert!((0.003297 - res).abs() < 0.0001, "res is {}", res);
    assert!(res < 0.0033);
    assert!(0.0032 < res);

    let img1b = d.create_image(&Img::new(buf1, file1.width, file1.height)).unwrap();
    let (res, _) = d.compare(&img1, img1b);

    assert!(0.000000000000001 > res);
    assert!(res < 0.000000000000001);
    assert_eq!(res, res);
}

