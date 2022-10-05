use std::{collections::HashMap, error::Error, iter::FromIterator, sync::Arc};

use parking_lot::Mutex;
use reqwest::Client;

use crate::{structs::{Config, FoundParameter, InjectionPlace, Stable, Parameters}, utils::{write_banner_response, empty_reqs, random_line, verify, self, replay}, network::{request::{RequestDefaults, Request}, response::Response}};

pub struct Runner<'a> {
    pub config: &'a Config,
    pub request_defaults: RequestDefaults,
    replay_client: &'a Client,
    pub params: Vec<String>,
    default_max: isize,

    pub max: usize,
    pub stable: Stable,
    pub initial_response: Response<'a>,

    //shared_info: SharedInfo<'a>,
    pub diffs: Vec<String>,
}

impl<'a> Runner<'a> {

    /// creates a runner, makes an initial response
    pub async fn new(
        config: &'a Config,
        request_defaults: &'a mut RequestDefaults,
        replay_client: &'a Client,
        params: &'a mut Vec<String>,
        mut default_max: isize
    ) -> Result<Runner<'a>, Box<dyn Error>> {
         //default_max can be negative in case guessed automatically.
         let mut max = default_max.abs() as usize;


         //make first request and collect some information like code, reflections, possible parameters
         //we are making another request defaults because the original one will be changed right after

         let mut temp_request_defaults = request_defaults.clone();

         //we need a random_parameter with a long value in order to increase accuracy while determining the default amount of reflections
         let random_parameters = vec![(random_line(10), random_line(10))];

         temp_request_defaults.parameters = random_parameters;

         let initial_response = Request::new(&temp_request_defaults, vec![])
                                                 .send()
                                                 .await?;

         //add possible parameters to the list of parameters in case the injection place is not headers
         if request_defaults.injection_place != InjectionPlace::Headers {
             for param in initial_response.get_possible_parameters() {
                 if !params.contains(&param) {
                     params.push(param)
                 }
             }
         }

         //in case the list is too small - change the max amount of parameters
         if params.len() < max {
             max = params.len();
             default_max = params.len() as isize;
             if max == 0 {
                 Err("No parameters were provided.")?
             }
         };

         //find how many times reflected supplied
         request_defaults.amount_of_reflections = initial_response.count(&temp_request_defaults.parameters.iter().next().unwrap().0);

         //TODO move to main whether to write or not
         /*if config.verbose > 0 && first_run {
             write_banner_response(&initial_response, self.request_defaults.amount_of_reflections, &self.params);
         }*/

         //some "magic" to be able to return initial_response
         //turns out you can't simple do 'initial_response.request = None'.
         //otherwise throws lifetime errors
         let initial_response = Response{
             time: initial_response.time,
             code: initial_response.code,
             headers: initial_response.headers,
             text: initial_response.text,
             reflected_parameters: initial_response.reflected_parameters,
             additional_parameter: initial_response.additional_parameter,
             request: None
         };

         /*let shared_info = SharedInfo{
            diffs: Arc::new(Mutex::new(&mut Vec::new())),
            found_params: Arc::new(Mutex::new(&mut Vec::new())),
            green_lines: Arc::new(Mutex::new(&mut Vec::new())),
         };*/

         Ok(
             Runner{
                 config,
                 request_defaults: request_defaults.clone(),
                 replay_client,
                 params: params.to_vec(),
                 default_max,
                 max: default_max.abs() as usize,
                 stable: Default::default(),
                 initial_response: initial_response,
                 //shared_info,
                 diffs: Vec::new(),
             }
         )
    }

    /// acually runs the runner
    async fn run(mut self, params: &Vec<String>) -> Result<(), Box<dyn Error>> {

        self.stability_checker().await?;

        let (diffs, mut found_params) = self.check_parameters(params).await?;

        found_params.append(&mut self.check_non_random_parameters().await?);

        //in case, for example, 'admin' param is found -- remove params like 'admin=true' or sth
        //TODO maybe check for the kind of parameter as well
        let mut found_params =
            found_params.iter().filter(|x|
                !(x.name.contains('=') && found_params.contains_key(x.name.split('=').next().unwrap()))
            ).map(|x| x.to_owned()).collect();

        //verify found parameters
        if self.config.verify {
            found_params = if let Ok(filtered_params)
                = verify(&self.initial_response, &self.request_defaults, &found_params, &diffs, &self.stable).await {
                filtered_params
            } else {
                utils::info(&self.config, "~", "was unable to verify found parameters");
                found_params
            };
        }

        if !self.config.replay_proxy.is_empty() {
            if let Err(_) = replay(&self.config, &self.request_defaults, &self.replay_client, &found_params).await {
                utils::info(&self.config, "~", "was unable to resend found parameters via different proxy");
            }
        }

        Ok(())
    }

    //check parameters like admin=true
    async fn check_non_random_parameters(&self) -> Result<Vec<FoundParameter>, Box<dyn Error>> {

        let mut found_parameters = Vec::new();

        if !self.config.disable_custom_parameters {
            let mut custom_parameters = self.config.custom_parameters.clone();
            let mut params = Vec::new();

            // in a loop check common parameters like debug, admin, .. with common values true, 1, false..
            // until there's no values left
            loop {
                for (k, v) in custom_parameters.iter_mut() {
                    if !v.is_empty() {
                        params.push([k.as_str(), "=", v.pop().unwrap().as_str()].concat());
                    }
                }

                if params.is_empty() {
                    break;
                }

                found_parameters.append(&mut self.check_parameters(&params).await?.1);
            }
        }

        Ok(found_parameters)
    }

    /// makes several requests in order to learn how the page behaves
    /// tries to increase the max amount of parameters per request in case the default value not changed
    async fn stability_checker(&mut self) -> Result<(), Box<dyn Error>> {
        //make a few requests and collect all persistent diffs, check for stability
        (self.diffs, self.stable) = empty_reqs(
            self.config,
            &self.initial_response,
            &self.request_defaults,
            self.config.learn_requests_count,
            self.max,
        ).await?;

        if self.config.reflected_only && !self.stable.reflections {
            Err("Reflections are not stable")?;
        }

        //check whether it is possible to use 192 or 256 params in a single request instead of 128 default
        if self.default_max == -128  {
            self.try_to_increase_max().await?;

            if self.max != self.default_max.abs() as usize {
                self.default_max = self.max as isize;
            }
        }

        Ok(())
    }

    /// checks whether the increasing of the amount of parameters changes the page
    /// changes self.max in case the page is stable with more parameters per request
    pub async fn try_to_increase_max(&mut self) -> Result<(), Box<dyn Error>> {
        let response = Request::new_random(&self.request_defaults, self.max + 64)
                                    .send()
                                    .await?;

        let (is_code_different, new_diffs) = response.compare(&self.initial_response, &self.diffs)?;
        let mut is_the_body_the_same = true;

        if !new_diffs.is_empty() {
            is_the_body_the_same = false;
        }

        //in case the page isn't different from previous one - try to increase max amount of parameters by 128
        if !is_code_different && (!self.stable.body || is_the_body_the_same) {

            let response =  Request::new_random(&self.request_defaults, self.max + 128)
                    .send()
                    .await?;

            let (is_code_different, new_diffs) = response.compare(&self.initial_response, &self.diffs)?;

            if !new_diffs.is_empty() {
                is_the_body_the_same = false;
            }

            if !is_code_different && (!self.stable.body || is_the_body_the_same) {
                self.max += 128
            } else {
                self.max += 64
            }

        }

        Ok(())
    }
}