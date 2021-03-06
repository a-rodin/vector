use crate::buffers::Acker;
use crate::{
    event::{self, Event, LogEvent, ValueKind},
    region::RegionOrEndpoint,
    sinks::util::{BatchServiceSink, SinkExt},
};
use futures::{sync::oneshot, try_ready, Async, Future, Poll};
use rusoto_core::RusotoFuture;
use rusoto_logs::{
    CloudWatchLogs, CloudWatchLogsClient, DescribeLogStreamsError, DescribeLogStreamsRequest,
    DescribeLogStreamsResponse, InputLogEvent, PutLogEventsError, PutLogEventsRequest,
    PutLogEventsResponse,
};
use serde::{Deserialize, Serialize};
use std::convert::TryInto;
use std::fmt;
use std::time::Duration;
use tower::{Service, ServiceBuilder};

pub struct CloudwatchLogsSvc {
    client: CloudWatchLogsClient,
    state: State,
    config: CloudwatchLogsSinkConfig,
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
pub struct CloudwatchLogsSinkConfig {
    pub stream_name: String,
    pub group_name: String,
    #[serde(flatten)]
    pub region: RegionOrEndpoint,
    pub batch_timeout: Option<u64>,
    pub batch_size: Option<usize>,
    pub encoding: Option<Encoding>,

    // Tower Request based configuration
    pub request_in_flight_limit: Option<usize>,
    pub request_timeout_secs: Option<u64>,
    pub request_rate_limit_duration_secs: Option<u64>,
    pub request_rate_limit_num: Option<u64>,
}

#[derive(Deserialize, Serialize, Debug, Eq, PartialEq, Clone)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    Text,
    Json,
}

enum State {
    Idle,
    Token(Option<String>),
    Describe(RusotoFuture<DescribeLogStreamsResponse, DescribeLogStreamsError>),
    Put(oneshot::Receiver<PutLogEventsResponse>),
}

#[derive(Debug)]
pub enum CloudwatchError {
    Put(PutLogEventsError),
    Describe(DescribeLogStreamsError),
    NoStreamsFound,
    ServiceDropped,
}

#[typetag::serde(name = "aws_cloudwatch_logs")]
impl crate::topology::config::SinkConfig for CloudwatchLogsSinkConfig {
    fn build(&self, acker: Acker) -> Result<(super::RouterSink, super::Healthcheck), String> {
        let cloudwatch = CloudwatchLogsSvc::new(self.clone())?;

        let timeout = self.request_timeout_secs.unwrap_or(60);
        let in_flight_limit = self.request_in_flight_limit.unwrap_or(5);
        let rate_limit_duration = self.request_rate_limit_duration_secs.unwrap_or(1);
        let rate_limit_num = self.request_rate_limit_num.unwrap_or(5);

        let batch_timeout = self.batch_timeout.unwrap_or(1);
        let batch_size = self.batch_size.unwrap_or(bytesize::mib(1u64) as usize);

        let svc = ServiceBuilder::new()
            .concurrency_limit(in_flight_limit)
            .rate_limit(rate_limit_num, Duration::from_secs(rate_limit_duration))
            .timeout(Duration::from_secs(timeout))
            .service(cloudwatch);

        let sink = {
            let svc_sink = BatchServiceSink::new(svc, acker).batched_with_min(
                Vec::new(),
                batch_size,
                Duration::from_secs(batch_timeout),
            );
            Box::new(svc_sink)
        };

        let healthcheck = healthcheck(self.clone())?;

        Ok((sink, healthcheck))
    }
}

impl CloudwatchLogsSvc {
    pub fn new(config: CloudwatchLogsSinkConfig) -> Result<Self, String> {
        let region = config.region.clone().try_into()?;
        let client = CloudWatchLogsClient::new(region);

        Ok(CloudwatchLogsSvc {
            client,
            config,
            state: State::Idle,
        })
    }

    fn put_logs(
        &mut self,
        sequence_token: Option<String>,
        events: Vec<Event>,
    ) -> RusotoFuture<PutLogEventsResponse, PutLogEventsError> {
        let log_events = events
            .into_iter()
            .map(Event::into_log)
            .map(|e| self.encode_log(e))
            .collect();

        let request = PutLogEventsRequest {
            log_events,
            sequence_token,
            log_group_name: self.config.group_name.clone(),
            log_stream_name: self.config.stream_name.clone(),
        };

        self.client.put_log_events(request)
    }

    fn describe_stream(
        &mut self,
    ) -> RusotoFuture<DescribeLogStreamsResponse, DescribeLogStreamsError> {
        let request = DescribeLogStreamsRequest {
            limit: Some(1),
            log_group_name: self.config.group_name.clone(),
            log_stream_name_prefix: Some(self.config.stream_name.clone()),
            ..Default::default()
        };

        self.client.describe_log_streams(request)
    }

    pub fn encode_log(&self, mut log: LogEvent) -> InputLogEvent {
        let timestamp = if let Some(ValueKind::Timestamp(ts)) = log.remove(&event::TIMESTAMP) {
            ts.timestamp_millis()
        } else {
            chrono::Utc::now().timestamp_millis()
        };

        match (&self.config.encoding, log.is_structured()) {
            (&Some(Encoding::Json), _) | (_, true) => {
                let bytes = serde_json::to_vec(&log.all_fields()).unwrap();
                let message = String::from_utf8(bytes).unwrap();

                InputLogEvent { message, timestamp }
            }
            (&Some(Encoding::Text), _) | (_, false) => {
                let message = log
                    .get(&event::MESSAGE)
                    .map(|v| v.to_string_lossy())
                    .unwrap_or_else(|| "".into());
                InputLogEvent { message, timestamp }
            }
        }
    }
}

impl Service<Vec<Event>> for CloudwatchLogsSvc {
    type Response = ();
    type Error = CloudwatchError;
    type Future = Box<dyn Future<Item = Self::Response, Error = Self::Error> + Send + 'static>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        loop {
            match &mut self.state {
                State::Idle => {
                    let fut = self.describe_stream();
                    self.state = State::Describe(fut);
                    continue;
                }
                State::Describe(fut) => {
                    let response = try_ready!(fut.poll().map_err(CloudwatchError::Describe));

                    let stream = response
                        .log_streams
                        .ok_or(CloudwatchError::NoStreamsFound)?
                        .into_iter()
                        .next()
                        .ok_or(CloudwatchError::NoStreamsFound)?;

                    self.state = State::Token(stream.upload_sequence_token);
                    return Ok(Async::Ready(()));
                }
                State::Token(_) => return Ok(Async::Ready(())),
                State::Put(fut) => {
                    let response = match fut.poll() {
                        Ok(Async::Ready(response)) => response,
                        Ok(Async::NotReady) => return Ok(Async::NotReady),
                        Err(_) => panic!("The in flight future was dropped!"),
                    };

                    self.state = State::Token(response.next_sequence_token);
                    return Ok(Async::Ready(()));
                }
            }
        }
    }

    fn call(&mut self, req: Vec<Event>) -> Self::Future {
        match &mut self.state {
            State::Token(token) => {
                let token = token.take();
                let (tx, rx) = oneshot::channel();
                self.state = State::Put(rx);

                let fut = self
                    .put_logs(token, req)
                    .map_err(CloudwatchError::Put)
                    .and_then(move |res| tx.send(res).map_err(|_| CloudwatchError::ServiceDropped));

                Box::new(fut)
            }
            _ => panic!("You did not call poll_ready!"),
        }
    }
}

fn healthcheck(config: CloudwatchLogsSinkConfig) -> Result<super::Healthcheck, String> {
    let region = config.region.clone();

    let client = CloudWatchLogsClient::new(region.try_into()?);

    let request = DescribeLogStreamsRequest {
        limit: Some(1),
        log_group_name: config.group_name.clone(),
        log_stream_name_prefix: Some(config.stream_name.clone()),
        ..Default::default()
    };

    let expected_stream = config.stream_name.clone();

    let fut = client
        .describe_log_streams(request)
        .map_err(|e| format!("DescribeLogStreams failed: {}", e))
        .and_then(|response| {
            response
                .log_streams
                .ok_or_else(|| "No streams found".to_owned())
        })
        .and_then(|streams| {
            streams
                .into_iter()
                .next()
                .ok_or_else(|| "No streams found".to_owned())
        })
        .and_then(|stream| {
            stream
                .log_stream_name
                .ok_or_else(|| "No stream name found but found a stream".to_owned())
        })
        .and_then(move |stream_name| {
            if stream_name == expected_stream {
                Ok(())
            } else {
                Err(format!(
                    "Stream returned is not the same as the one passed in got: {}, expected: {}",
                    stream_name, expected_stream
                ))
            }
        });

    Ok(Box::new(fut))
}

impl fmt::Display for CloudwatchError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            CloudwatchError::Put(e) => write!(f, "CloudwatchError::Put: {}", e),
            CloudwatchError::Describe(e) => write!(f, "CloudwatchError::Describe: {}", e),
            CloudwatchError::NoStreamsFound => write!(f, "CloudwatchError: No Streams Found"),
            CloudwatchError::ServiceDropped => write!(
                f,
                "CloudwatchError: The service was dropped while there was a request in flight."
            ),
        }
    }
}

impl std::error::Error for CloudwatchError {}

impl From<PutLogEventsError> for CloudwatchError {
    fn from(e: PutLogEventsError) -> Self {
        CloudwatchError::Put(e)
    }
}

impl From<DescribeLogStreamsError> for CloudwatchError {
    fn from(e: DescribeLogStreamsError) -> Self {
        CloudwatchError::Describe(e)
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        event::{self, Event, ValueKind},
        region::RegionOrEndpoint,
        sinks::aws_cloudwatch_logs::{CloudwatchLogsSinkConfig, CloudwatchLogsSvc},
    };
    use std::collections::HashMap;
    use string_cache::DefaultAtom as Atom;

    #[test]
    fn cloudwatch_encode_log() {
        let config = CloudwatchLogsSinkConfig {
            region: RegionOrEndpoint::with_endpoint("http://localhost:6000".into()),
            ..Default::default()
        };
        let svc = CloudwatchLogsSvc::new(config).unwrap();

        let mut event = Event::from("hello world").into_log();

        event.insert_explicit("key".into(), "value".into());

        let input_event = svc.encode_log(event.clone());

        let ts = if let ValueKind::Timestamp(ts) = event[&event::TIMESTAMP] {
            ts.timestamp_millis()
        } else {
            panic!()
        };

        assert_eq!(input_event.timestamp, ts);

        let bytes = input_event.message;

        let map: HashMap<Atom, String> = serde_json::from_str(&bytes[..]).unwrap();

        assert!(map.get(&event::TIMESTAMP).is_none());
    }
}

#[cfg(feature = "cloudwatch-integration-tests")]
#[cfg(test)]
mod integration_tests {

    use crate::buffers::Acker;
    use crate::{
        region::RegionOrEndpoint,
        sinks::aws_cloudwatch_logs::CloudwatchLogsSinkConfig,
        test_util::{block_on, random_lines_with_stream},
        topology::config::SinkConfig,
    };
    use futures::Sink;
    use rusoto_core::Region;
    use rusoto_logs::{
        CloudWatchLogs, CloudWatchLogsClient, CreateLogGroupRequest, CreateLogStreamRequest,
        GetLogEventsRequest,
    };

    const STREAM_NAME: &'static str = "test-1";
    const GROUP_NAME: &'static str = "router";

    #[test]
    fn cloudwatch_insert_log_event() {
        let region = Region::Custom {
            name: "localstack".into(),
            endpoint: "http://localhost:6000".into(),
        };
        ensure_stream(region.clone());

        let config = CloudwatchLogsSinkConfig {
            stream_name: STREAM_NAME.into(),
            group_name: GROUP_NAME.into(),
            region: RegionOrEndpoint::with_endpoint("http://localhost:6000".into()),
            ..Default::default()
        };

        let (sink, _) = config.build(Acker::Null).unwrap();

        let timestamp = chrono::Utc::now();

        let (input_lines, events) = random_lines_with_stream(100, 11);

        let pump = sink.send_all(events);
        block_on(pump).unwrap();

        let mut request = GetLogEventsRequest::default();
        request.log_stream_name = STREAM_NAME.into();
        request.log_group_name = GROUP_NAME.into();
        request.start_time = Some(timestamp.timestamp_millis());

        std::thread::sleep(std::time::Duration::from_millis(1000));

        let client = CloudWatchLogsClient::new(region);

        let response = block_on(client.get_log_events(request)).unwrap();

        let events = response.events.unwrap();

        let output_lines = events
            .into_iter()
            .map(|e| e.message.unwrap())
            .collect::<Vec<_>>();

        assert_eq!(output_lines, input_lines);
    }

    fn ensure_stream(region: Region) {
        let client = CloudWatchLogsClient::new(region);

        let req = CreateLogGroupRequest {
            log_group_name: GROUP_NAME.into(),
            ..Default::default()
        };

        match client.create_log_group(req).sync() {
            Ok(_) => (),
            Err(_) => (),
        };

        let req = CreateLogStreamRequest {
            log_group_name: GROUP_NAME.into(),
            log_stream_name: STREAM_NAME.into(),
        };

        match client.create_log_stream(req).sync() {
            Ok(_) => (),
            Err(_) => (),
        };
    }

}
